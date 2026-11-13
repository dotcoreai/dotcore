//! The joining node's temporary listener on its private port, which the manager dials back to
//! check that it reaches this node before it approves the join.
//!
//! The listener binds the address and port the node folder will record, so a successful dial-back
//! proves the address the node later serves on. It presents a self-signed certificate of the
//! node's own key over TLS 1.3, completes each handshake and closes it; it serves nothing else and
//! stops once the join has its answer.

use std::{io, net::SocketAddr, sync::Arc};

use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{NodeJoinError, NodeKey};

/// A running dial-back listener. Dropping it stops it in the background; [`Self::stop`] also
/// waits until its port is released.
pub(super) struct DialBackListener {
    address: SocketAddr,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl DialBackListener {
    /// Binds `address` and starts answering TLS handshakes with the self-signed certificate of
    /// `key`. Fails when the address cannot be bound, for example because something else listens
    /// on the port.
    pub(super) async fn start(address: SocketAddr, key: &NodeKey) -> Result<Self, NodeJoinError> {
        let acceptor = TlsAcceptor::from(Arc::new(key.dial_back_server_config()?));
        let listener = TcpListener::bind(address)
            .await
            .map_err(|source| NodeJoinError::PrivateListener { address, source })?;
        let address = listener.local_addr().unwrap_or(address);
        info!(%address, "listening on the private port for the manager's dial-back");
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(listener, acceptor, stop.clone()));
        Ok(Self {
            address,
            stop,
            task: Some(task),
        })
    }

    /// Returns the address the listener is bound to.
    #[cfg(test)]
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Stops listening and waits until the port is released, so the node can bind it again.
    pub(super) async fn stop(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        debug!(address = %self.address, "stopped listening for the manager's dial-back");
    }
}

impl Drop for DialBackListener {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Accepts connections until `stop`, completing one TLS handshake on each. Pending handshakes
/// are dropped with the listener.
async fn serve(listener: TcpListener, acceptor: TlsAcceptor, stop: CancellationToken) {
    let mut handshakes = JoinSet::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            Some(_) = handshakes.join_next(), if !handshakes.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    handshakes.spawn(handshake(acceptor.clone(), stream, peer));
                }
                // A connection reset before it was accepted concerns only that connection.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => {
                    warn!(%error, "the dial-back listener stopped accepting connections");
                    return;
                }
            },
        }
    }
}

/// Completes one TLS handshake, which proves the node key to the dialer, and closes the
/// connection.
async fn handshake(acceptor: TlsAcceptor, stream: TcpStream, peer: SocketAddr) {
    match acceptor.accept(stream).await {
        Ok(mut stream) => {
            debug!(%peer, "dial-back handshake completed");
            let _ = stream.shutdown().await;
        }
        Err(error) => debug!(%peer, %error, "dial-back handshake failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use rustls::{
        ClientConfig, DigitallySignedStruct, SignatureScheme,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
        pki_types::{CertificateDer, ServerName, UnixTime},
    };
    use tokio_rustls::TlsConnector;

    use super::*;
    use crate::membership::join::node_key::{certificate_key, csr_public_key};

    #[tokio::test]
    async fn the_listener_proves_the_signing_request_key_over_tls_13_only() {
        let key = NodeKey::generate().expect("key");
        let listener = DialBackListener::start(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), &key)
            .await
            .expect("listener");
        let csr_key = csr_public_key(&key.csr_der().expect("csr")).expect("csr key");

        let presented = handshake_with(listener.address(), &[&rustls::version::TLS13])
            .await
            .expect("TLS 1.3 handshake");
        assert_eq!(certificate_key(&presented), Some(csr_key));

        let refused = handshake_with(listener.address(), &[&rustls::version::TLS12]).await;
        assert!(refused.is_err(), "a TLS 1.2 handshake must fail");
        listener.stop().await;
    }

    #[tokio::test]
    async fn a_taken_address_is_reported_and_a_stopped_listener_frees_it() {
        let key = NodeKey::generate().expect("key");
        let first = DialBackListener::start(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), &key)
            .await
            .expect("first listener");
        let address = first.address();

        let error = DialBackListener::start(address, &key)
            .await
            .err()
            .expect("the address is taken");
        assert!(
            matches!(
                &error,
                NodeJoinError::PrivateListener { address: tried, source }
                    if *tried == address && source.kind() == io::ErrorKind::AddrInUse
            ),
            "{error:?}"
        );

        first.stop().await;
        DialBackListener::start(address, &key)
            .await
            .expect("the stopped listener released its address")
            .stop()
            .await;
    }

    /// Completes a handshake with the listener at `address` limited to `versions`, and returns
    /// the certificate it presented.
    async fn handshake_with(
        address: SocketAddr,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Result<Vec<u8>, io::Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(AnyCertificate(Arc::clone(&provider)));
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let stream = TcpStream::connect(address).await?;
        let stream = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("node").expect("name"), stream)
            .await?;
        let (_, connection) = stream.get_ref();
        Ok(connection
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .expect("presented certificate")
            .to_vec())
    }

    /// Accepts any certificate but still checks the handshake signature against it.
    #[derive(Debug)]
    struct AnyCertificate(Arc<CryptoProvider>);

    impl ServerCertVerifier for AnyCertificate {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            certificate: &CertificateDer<'_>,
            signature: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                certificate,
                signature,
                &self.0.signature_verification_algorithms,
            )
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
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}
