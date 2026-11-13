//! The manager's dial-back to a joining node: before the operator reviews a join, the manager
//! connects to the host and private port it would record and checks that the node answering
//! there holds the key of the node's signing request.
//!
//! A successful dial-back proves both reachability from the manager's machine and the node's
//! identity, since the TLS handshake signature can only come from that key; neither the dialed
//! address nor any name in the certificate counts. A bounded number of resolved addresses is
//! tried, each within its share of one overall bound.

use std::{
    error::Error as _,
    io,
    net::{SocketAddr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dotcore_domain::{HostResolutionError, HostResolver, SiteNodeHost, real_time};
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::Instant};
use tokio_rustls::TlsConnector;
use tracing::debug;

use super::node_key::certificate_key;

/// One dial-back the join host made, reported to the operator whatever its outcome, so a token
/// holder's attempts to make the manager connect somewhere stay visible.
#[derive(Debug)]
pub struct DialBack {
    /// Address or host name the site would record for the node.
    pub host: SiteNodeHost,

    /// Private port the node proposed.
    pub port: u16,

    /// The address that answered with the node's key, or why none did.
    pub outcome: Result<SocketAddr, DialBackError>,
}

/// Most resolved addresses one dial-back tries, so a single counted failure cannot make the
/// manager's machine probe many addresses.
pub(super) const MAX_DIAL_BACK_ADDRESSES: usize = 4;

/// Why the manager did not reach the joining node by its key.
#[derive(Debug, thiserror::Error)]
pub enum DialBackError {
    #[error("the name does not resolve on the manager's machine")]
    NotResolved(#[source] HostResolutionError),

    #[error("{0}")]
    Connect(io::ErrorKind),

    #[error("timed out")]
    TimedOut,

    #[error("another key answered")]
    AnotherKey,

    #[error("the TLS handshake failed")]
    Handshake(#[source] io::Error),
}

impl DialBackError {
    /// Describes the failure with its underlying causes, for the joining node and the operator.
    pub fn detailed(&self) -> String {
        let mut text = self.to_string();
        let mut cause = self.source();
        while let Some(error) = cause {
            text.push_str(&format!(": {error}"));
            cause = error.source();
        }
        text
    }

    /// How much the failure says about the address dialed; a later attempt keeps the most
    /// specific one.
    fn specificity(&self) -> u8 {
        match self {
            Self::AnotherKey => 4,
            Self::Handshake(_) => 3,
            Self::Connect(_) => 2,
            Self::NotResolved(_) => 1,
            Self::TimedOut => 0,
        }
    }
}

/// Connects to `host` at `port` and completes a TLS 1.3 handshake proving `node_key`, the raw
/// public key of the node's signing request. Up to [`MAX_DIAL_BACK_ADDRESSES`] of the addresses
/// the host resolves to are tried in turn, each within an equal share of the time left in
/// `timeout`, until one proves the key.
pub(super) async fn dial_back(
    host: &SiteNodeHost,
    port: u16,
    node_key: &[u8],
    resolver: &dyn HostResolver,
    timeout: Duration,
) -> Result<SocketAddr, DialBackError> {
    let deadline = Instant::now() + timeout;
    let addresses = real_time::bound_until(deadline, host.resolve_all(resolver))
        .await
        .map_err(|_| DialBackError::TimedOut)?
        .map_err(DialBackError::NotResolved)?;
    let addresses: Vec<_> = addresses
        .into_iter()
        .take(MAX_DIAL_BACK_ADDRESSES)
        .collect();
    let mut failure = DialBackError::TimedOut;
    for (index, address) in addresses.iter().enumerate() {
        let address = SocketAddr::V4(SocketAddrV4::new(*address, port));
        let left = u32::try_from(addresses.len() - index).unwrap_or(1);
        let share = deadline.saturating_duration_since(Instant::now()) / left;
        let error = match real_time::bound(share, handshake(address, node_key)).await {
            Ok(Ok(())) => return Ok(address),
            Ok(Err(error)) => error,
            Err(_) => DialBackError::TimedOut,
        };
        debug!(%host, %address, error = %error.detailed(), "dial-back attempt failed");
        if error.specificity() > failure.specificity() {
            failure = error;
        }
    }
    Err(failure)
}

/// Completes one handshake with `address`, requiring the presented certificate to carry
/// `node_key` and the handshake to be signed by it.
async fn handshake(address: SocketAddr, node_key: &[u8]) -> Result<(), DialBackError> {
    let verifier = Arc::new(NodeKeyVerifier::new(node_key));
    let config = ClientConfig::builder_with_provider(Arc::clone(&verifier.provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| DialBackError::Handshake(io::Error::other(error)))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(&verifier) as _)
        .with_no_client_auth();
    let stream = TcpStream::connect(address)
        .await
        .map_err(|error| DialBackError::Connect(error.kind()))?;
    match TlsConnector::from(Arc::new(config))
        .connect(ServerName::IpAddress(address.ip().into()), stream)
        .await
    {
        Ok(mut stream) => {
            let _ = stream.shutdown().await;
            Ok(())
        }
        Err(_) if verifier.another_key.load(Ordering::SeqCst) => Err(DialBackError::AnotherKey),
        Err(error) => Err(DialBackError::Handshake(error)),
    }
}

/// Server verifier pinned to one raw public key: the leaf must carry exactly that key, and the
/// TLS 1.3 handshake signature must verify against it.
///
/// Names and validity dates are not checked; the joining node's certificate is self-signed and
/// only proves possession of its key.
#[derive(Debug)]
struct NodeKeyVerifier {
    node_key: Vec<u8>,
    provider: Arc<CryptoProvider>,
    another_key: AtomicBool,
}

impl NodeKeyVerifier {
    fn new(node_key: &[u8]) -> Self {
        Self {
            node_key: node_key.to_vec(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            another_key: AtomicBool::new(false),
        }
    }
}

impl ServerCertVerifier for NodeKeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if certificate_key(end_entity.as_ref()).as_deref() != Some(self.node_key.as_slice()) {
            self.another_key.store(true, Ordering::SeqCst);
            return Err(rustls::Error::General(
                "the dial-back answer does not carry the joining node's key".to_owned(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _certificate: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "the dial-back accepts TLS 1.3 only".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::test_support::host_resolver::FakeHostResolver;
    use tokio::net::TcpListener;

    use super::*;
    use crate::membership::join::{
        NodeKey, dial_back_listener::DialBackListener, node_key::csr_public_key,
    };

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn the_node_listener_proves_its_key_and_any_other_key_is_refused() {
        let node = NodeKey::generate().expect("node key");
        let other = NodeKey::generate().expect("other key");
        let listener = DialBackListener::start(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), &node)
            .await
            .expect("listener");
        let port = listener.address().port();
        let host = SiteNodeHost::LOOPBACK;
        let resolver = FakeHostResolver::default();
        let key_of = |key: &NodeKey| csr_public_key(&key.csr_der().expect("csr")).expect("key");

        let reached = dial_back(&host, port, &key_of(&node), &resolver, TIMEOUT).await;
        assert_eq!(
            reached.expect("the node's key"),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port))
        );
        let refused = dial_back(&host, port, &key_of(&other), &resolver, TIMEOUT).await;
        assert!(
            matches!(refused, Err(DialBackError::AnotherKey)),
            "{refused:?}"
        );
        listener.stop().await;
    }

    #[tokio::test]
    async fn a_silent_listener_times_out_and_an_unknown_name_does_not_resolve() {
        let node = NodeKey::generate().expect("node key");
        let key = csr_public_key(&node.csr_der().expect("csr")).expect("key");
        let silent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("silent listener");
        let port = silent.local_addr().expect("address").port();
        let resolver = FakeHostResolver::default();

        let timed_out = dial_back(
            &SiteNodeHost::LOOPBACK,
            port,
            &key,
            &resolver,
            Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(timed_out, Err(DialBackError::TimedOut)),
            "{timed_out:?}"
        );

        let unknown = SiteNodeHost::parse("unknown.example").expect("host");
        let unresolved = dial_back(&unknown, port, &key, &resolver, TIMEOUT).await;
        assert_eq!(
            unresolved.expect_err("unknown name").to_string(),
            "the name does not resolve on the manager's machine"
        );
        drop(silent);
    }

    #[tokio::test]
    async fn a_silent_address_leaves_time_for_the_next_and_another_key_outranks_a_timeout() {
        // A second loopback address, which Linux has and macOS has only with an `lo0` alias.
        const SILENT: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 5);
        if std::net::TcpListener::bind((SILENT, 0)).is_err() {
            eprintln!("skipped: {SILENT} is not usable on this machine");
            return;
        }
        let node = NodeKey::generate().expect("node key");
        let key = csr_public_key(&node.csr_der().expect("csr")).expect("key");
        let listener = DialBackListener::start(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), &node)
            .await
            .expect("listener");
        let port = listener.address().port();
        let _silent = std::net::TcpListener::bind((SILENT, port)).expect("silent listener");
        let host = SiteNodeHost::parse("multi.example").expect("host");
        let silent_first =
            FakeHostResolver::default().with("multi.example", [SILENT, Ipv4Addr::LOCALHOST]);
        let timeout = Duration::from_secs(2);

        let reached = dial_back(&host, port, &key, &silent_first, timeout).await;
        assert_eq!(
            reached.expect("the second address within its share"),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port))
        );

        let other = NodeKey::generate().expect("other key");
        let other_key = csr_public_key(&other.csr_der().expect("csr")).expect("key");
        let key_first =
            FakeHostResolver::default().with("multi.example", [Ipv4Addr::LOCALHOST, SILENT]);
        let failed = dial_back(&host, port, &other_key, &key_first, timeout).await;
        assert!(
            matches!(failed, Err(DialBackError::AnotherKey)),
            "{failed:?}"
        );
        listener.stop().await;
    }

    #[tokio::test]
    async fn at_most_the_address_cap_is_dialed() {
        let node = NodeKey::generate().expect("node key");
        let key = csr_public_key(&node.csr_der().expect("csr")).expect("key");
        let refusing = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener that closes every connection");
        let port = refusing.local_addr().expect("address").port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&accepted);
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = refusing.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        let many = FakeHostResolver::default().with(
            "many.example",
            std::iter::repeat_n(Ipv4Addr::LOCALHOST, MAX_DIAL_BACK_ADDRESSES + 3),
        );
        let host = SiteNodeHost::parse("many.example").expect("host");

        let failed = dial_back(&host, port, &key, &many, TIMEOUT).await;

        let error = failed.expect_err("nothing proves the key");
        assert!(matches!(error, DialBackError::Handshake(_)), "{error:?}");
        assert!(
            error.detailed().starts_with("the TLS handshake failed: "),
            "{}",
            error.detailed()
        );
        assert_eq!(accepted.load(Ordering::SeqCst), MAX_DIAL_BACK_ADDRESSES);
        server.abort();
    }
}
