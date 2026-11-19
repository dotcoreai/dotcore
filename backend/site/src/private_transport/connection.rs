//! Accepts the TLS connections of a node's private listener, gives each one an identity and a
//! close notice, and bounds the connections whose certificate is not pinned.
//!
//! The listener presents the node's certificate with the site CA. The site manager's listener asks
//! for a client certificate without requiring one, so a machine that holds no site certificate yet
//! can reach the join service; every other node requires one. After the handshake, a connection
//! whose certificate is pinned is never capped or closed for being idle, and closes once its pin is
//! withdrawn or replaced. Any other connection
//! counts toward a fixed limit, closes once it stays too long without a join request in progress,
//! and gives way to a newer one at the limit unless it ever presented a valid join secret.

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
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
use tokio_util::sync::{CancellationToken, DropGuard, WaitForCancellationFutureOwned};
use tonic::transport::server::Connected;
use tracing::debug;

use super::{PeerPins, PrivateNodeRole, PrivateServerConfiguration};

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

    /// Unpinned connections open now, oldest first.
    unpinned: Arc<Mutex<Vec<UnpinnedEntry>>>,
}

impl PrivateConnections {
    /// Prepares acceptance for the listener described by `configuration`, classifying each
    /// connection against its live peer pins. Only a site manager's listener accepts a connection
    /// without a client certificate, since only it hosts joins.
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
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider));
        let verifier = match configuration.local_role {
            PrivateNodeRole::Manager => verifier.allow_unauthenticated(),
            PrivateNodeRole::Worker => verifier,
        }
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
            unpinned: Arc::default(),
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
        let revoked = peer_certificate.as_deref().and_then(|leaf| {
            self.pins
                .authenticate_revocable(&self.site_id, leaf)
                .map(|(_, revoked)| Box::pin(revoked.cancelled_owned()))
        });
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let activity = Arc::new(Mutex::new(ConnectionActivity {
            requests: 0,
            idle_since: Instant::now(),
            admitted: false,
            closing: Closing::Open,
            reader: None,
        }));
        let unpinned = if revoked.is_some() {
            None
        } else {
            Some(self.claim_unpinned(id, &activity, peer_address)?)
        };
        let closed = CancellationToken::new();
        Some(PrivateConnectionIo {
            stream,
            info: PrivateConnectionInfo {
                id,
                peer_address,
                peer_certificate,
                activity,
                closed: closed.clone(),
            },
            unpinned,
            revoked,
            _closed: closed.drop_guard(),
        })
    }

    /// Counts one more unpinned connection. At the limit it closes the oldest unpinned connection
    /// that never presented a valid join secret to make room, or refuses the new one when there
    /// is none, so connections held in advance can't keep a joining machine out.
    fn claim_unpinned(
        &self,
        id: u64,
        activity: &Arc<Mutex<ConnectionActivity>>,
        peer_address: SocketAddr,
    ) -> Option<UnpinnedSlot> {
        let mut open = lock(&self.unpinned);
        if open.len() >= self.limits.max_open {
            let Some(oldest) = open
                .iter()
                .position(|entry| !lock(&entry.activity).admitted)
            else {
                debug!(peer = %peer_address, "private connection refused: too many unpinned connections");
                return None;
            };
            let evicted = open.remove(oldest);
            lock(&evicted.activity).close_now();
            debug!(
                peer = %peer_address,
                evicted_connection = evicted.id,
                "private connection replaced the oldest unpinned connection that presented no join secret"
            );
        }
        open.push(UnpinnedEntry {
            id,
            activity: Arc::clone(activity),
        });
        Some(UnpinnedSlot {
            open: Arc::clone(&self.unpinned),
            id,
            idle_timeout: self.limits.idle_timeout,
            idle: Box::pin(real_time::deadline(
                Instant::now() + self.limits.idle_timeout,
            )),
        })
    }
}

/// Bounds on the private connections whose certificate is not pinned, such as a joining
/// machine's.
#[derive(Clone, Copy, Debug)]
pub struct UnpinnedConnectionLimits {
    /// Such connections open at once. A further one replaces the oldest that never presented a
    /// valid join secret, or is closed after its handshake when every one did.
    pub max_open: usize,

    /// Longest time such a connection stays open without a join request in progress: from its
    /// handshake to its first one, and between them. Traffic that is not a join request, such as
    /// a keepalive ping, doesn't count.
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

    /// Marks a join request that presented a valid secret. The idle timeout of an unpinned
    /// connection pauses until the returned guard drops, for a request that waits long, such as
    /// an operator review, and the connection no longer gives way to a newer one at the limit.
    pub fn begin_request(&self) -> RequestInProgress {
        let mut activity = self.lock_activity();
        activity.requests += 1;
        activity.admitted = true;
        RequestInProgress(Arc::clone(&self.activity))
    }

    /// Closes an unpinned connection once the response in flight has been sent, for a peer that
    /// presented a wrong join secret. A pinned connection stays open.
    pub fn close(&self) {
        let mut activity = self.lock_activity();
        if activity.closing == Closing::Open {
            activity.closing = Closing::AfterFlush;
        }
    }

    /// Completes once the connection has closed.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    fn lock_activity(&self) -> MutexGuard<'_, ConnectionActivity> {
        lock(&self.activity)
    }

    /// Returns the identity of a connection without a certificate that no listener accepted,
    /// for checks a test makes without one.
    #[cfg(test)]
    pub(crate) fn unattached(peer_address: SocketAddr) -> Self {
        Self {
            id: u64::MAX,
            peer_address,
            peer_certificate: None,
            activity: Arc::new(Mutex::new(ConnectionActivity {
                requests: 0,
                idle_since: Instant::now(),
                admitted: false,
                closing: Closing::Open,
                reader: None,
            })),
            closed: CancellationToken::new(),
        }
    }
}

/// Keeps the idle timeout of a connection paused while a request is running.
pub struct RequestInProgress(Arc<Mutex<ConnectionActivity>>);

impl Drop for RequestInProgress {
    fn drop(&mut self) {
        let mut activity = lock(&self.0);
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

    /// Completes once the pin that authenticated a pinned connection is withdrawn or replaced.
    revoked: Option<Pin<Box<WaitForCancellationFutureOwned>>>,

    // Dropped last, so the close notice fires once the unpinned slot is free.
    _closed: DropGuard,
}

impl PrivateConnectionIo {
    /// Fails a pending read of a pinned connection once its pin was revoked, and of an unpinned
    /// connection once it was closed, or once it stayed past its idle timeout without a join
    /// request in progress.
    fn poll_closed(&mut self, context: &mut Context<'_>) -> Poll<io::Error> {
        if let Some(revoked) = self.revoked.as_mut() {
            return revoked.as_mut().poll(context).map(|()| {
                io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the pin of this private connection was revoked",
                )
            });
        }
        let Some(unpinned) = self.unpinned.as_mut() else {
            return Poll::Pending;
        };
        let mut activity = lock(&self.info.activity);
        if activity.closing == Closing::Now {
            return Poll::Ready(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "unpinned private connection was closed",
            ));
        }
        if !activity
            .reader
            .as_ref()
            .is_some_and(|reader| reader.will_wake(context.waker()))
        {
            activity.reader = Some(context.waker().clone());
        }
        if activity.requests > 0 {
            return Poll::Pending;
        }
        let deadline = activity.idle_since + unpinned.idle_timeout;
        drop(activity);
        if unpinned.idle.deadline() != deadline {
            unpinned.idle.as_mut().reset(deadline);
        }
        unpinned.idle.as_mut().poll(context).map(|()| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "unpinned private connection was idle too long",
            )
        })
    }

    /// Lets a close requested for after the response in flight take effect once it was sent.
    fn record_flush(&self) {
        if self.unpinned.is_none() {
            return;
        }
        let mut activity = lock(&self.info.activity);
        if activity.closing == Closing::AfterFlush {
            activity.close_now();
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
        if let Poll::Ready(error) = this.poll_closed(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for PrivateConnectionIo {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let flushed = Pin::new(&mut this.stream).poll_flush(context);
        if matches!(flushed, Poll::Ready(Ok(()))) {
            this.record_flush();
        }
        flushed
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(context)
    }
}

/// What one connection is doing: its join requests in progress, when it last became idle,
/// whether it ever presented a valid join secret, and whether it is closing.
#[derive(Debug)]
struct ConnectionActivity {
    requests: usize,
    idle_since: Instant,
    admitted: bool,
    closing: Closing,

    /// Wakes the connection's pending read once it must close.
    reader: Option<Waker>,
}

impl ConnectionActivity {
    fn close_now(&mut self) {
        self.closing = Closing::Now;
        if let Some(reader) = self.reader.take() {
            reader.wake();
        }
    }
}

/// How far an unpinned connection is in closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closing {
    Open,

    /// Closes once the response in flight was sent.
    AfterFlush,

    /// Fails its next read, which ends the connection.
    Now,
}

/// One unpinned connection open now, as the limit sees it.
struct UnpinnedEntry {
    id: u64,
    activity: Arc<Mutex<ConnectionActivity>>,
}

/// The place one unpinned connection holds in the limit, with its idle timeout.
struct UnpinnedSlot {
    open: Arc<Mutex<Vec<UnpinnedEntry>>>,
    id: u64,
    idle_timeout: Duration,
    idle: Pin<Box<Sleep>>,
}

impl Drop for UnpinnedSlot {
    fn drop(&mut self) {
        lock(&self.open).retain(|entry| entry.id != self.id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
        drop(unpinned.connect_info().begin_request());
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

    #[tokio::test]
    async fn at_the_limit_a_new_connection_replaces_the_oldest_that_presented_no_secret() {
        let fixture = Fixture::new();
        let (address, mut accepted) = accept_private_connections(
            &fixture.configuration,
            UnpinnedConnectionLimits {
                max_open: 2,
                idle_timeout: DEADLINE * 6,
            },
        )
        .await;
        let mut next = async || {
            let client = fixture.connect(address, false).await;
            let server = tokio::time::timeout(DEADLINE, accepted.recv())
                .await
                .expect("an accepted connection")
                .expect("acceptance runs");
            (client, server)
        };
        let (_admitted_client, mut admitted) = next().await;
        drop(admitted.connect_info().begin_request());
        let (_first_client, mut first) = next().await;

        let (_second_client, mut second) = next().await;
        let mut byte = [0_u8; 1];
        let evicted = tokio::time::timeout(DEADLINE, first.read(&mut byte))
            .await
            .expect("the oldest connection without a secret is closed")
            .expect_err("evicted connection");
        assert_eq!(evicted.kind(), io::ErrorKind::ConnectionAborted);

        let (_third_client, _third) = next().await;
        let evicted = tokio::time::timeout(DEADLINE, second.read(&mut byte))
            .await
            .expect("the next oldest connection without a secret is closed")
            .expect_err("evicted connection");
        assert_eq!(evicted.kind(), io::ErrorKind::ConnectionAborted);
        let admitted_read = tokio::spawn(async move {
            let mut byte = [0_u8; 1];
            admitted.read(&mut byte).await
        });

        // Once every open connection presented a secret, a newcomer is refused instead.
        drop(_third);
        let (_fourth_client, fourth) = next().await;
        drop(fourth.connect_info().begin_request());
        let mut refused = fixture.connect(address, false).await;
        let read = tokio::time::timeout(DEADLINE, refused.read(&mut byte))
            .await
            .expect("the connection over the limit is closed");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        assert!(
            !admitted_read.is_finished(),
            "a connection that presented a secret stays open"
        );
        admitted_read.abort();
    }

    #[tokio::test]
    async fn a_closed_connection_sends_its_pending_response_then_ends() {
        let fixture = Fixture::new();
        let (address, mut accepted) = accept_private_connections(
            &fixture.configuration,
            UnpinnedConnectionLimits {
                max_open: 1,
                idle_timeout: DEADLINE * 6,
            },
        )
        .await;
        let mut client = fixture.connect(address, false).await;
        let mut server = tokio::time::timeout(DEADLINE, accepted.recv())
            .await
            .expect("an accepted connection")
            .expect("acceptance runs");

        server.connect_info().close();
        server
            .write_all(b"refused")
            .await
            .expect("write the response");
        server.flush().await.expect("send the response");
        let mut byte = [0_u8; 1];
        let closed = tokio::time::timeout(DEADLINE, server.read(&mut byte))
            .await
            .expect("the connection closes after its response")
            .expect_err("closed connection");
        assert_eq!(closed.kind(), io::ErrorKind::ConnectionAborted);

        let mut response = [0_u8; 7];
        client
            .read_exact(&mut response)
            .await
            .expect("the response arrives");
        assert_eq!(&response, b"refused");
    }

    #[tokio::test]
    async fn a_worker_listener_refuses_connections_without_a_certificate() {
        let fixture = Fixture::new();
        let worker = fixture.authority.private_configuration(
            "site-one",
            "worker-one",
            PrivateNodeRole::Worker,
            fixture.worker.clone(),
            [(
                "manager-node",
                PrivateNodeRole::Manager,
                fixture.manager_certificate.as_slice(),
            )],
        );
        let (address, mut accepted) = accept_private_connections(
            &worker,
            UnpinnedConnectionLimits {
                max_open: 1,
                idle_timeout: IDLE_TIMEOUT,
            },
        )
        .await;
        let verifier = PinnedPeerVerifier::new(
            fixture.authority.certificate.der(),
            "site-one",
            "worker-one",
            CertificateFingerprint::for_certificate(&fixture.worker.0),
        )
        .expect("worker verifier");

        // TLS 1.3 reports the server's refusal of a missing certificate on the first read.
        let refused = match connect_tls(address, Arc::new(verifier), None).await {
            Ok(mut stream) => {
                let mut byte = [0_u8; 1];
                tokio::time::timeout(DEADLINE, stream.read(&mut byte))
                    .await
                    .expect("the refusal arrives")
                    .map(|read| read > 0)
            }
            Err(error) => Err(error),
        };
        assert!(!matches!(refused, Ok(true)), "{refused:?}");
        assert!(
            accepted.try_recv().is_err(),
            "no connection without a certificate is accepted"
        );
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
