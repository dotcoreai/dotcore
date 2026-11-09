//! Transport-only gRPC-Web and inference HTTP server assembly and lifecycle.
//!
//! Protocol bindings are generated from root `proto/` schemas by `dotcore-proto`.

mod address;
mod cors;
mod inference;
mod lifecycle;
mod management;
pub mod private;

use std::{future::Future, io, pin::Pin};

use http::HeaderValue;
use tracing::{Instrument, error, field, info, info_span};

use crate::router::InferenceRouter;
pub(crate) use address::BoundTransports;
pub use address::ServerAddresses;

pub struct Server {
    management: Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>,
    inference: Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>,
    private: Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>,
    private_address: Option<std::net::SocketAddr>,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Server {
    /// Prepares opaque transport routes and listeners without product capability work.
    pub(crate) async fn prepare<F>(
        transports: BoundTransports,
        allowed_origins: Vec<HeaderValue>,
        routes: tonic::service::Routes,
        router: Option<InferenceRouter>,
        private: Option<private::PrivateListener>,
        shutdown: F,
    ) -> ServerResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let BoundTransports { public, .. } = transports;
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let (management, inference) = match (public, router) {
            (Some(listeners), Some(router)) => (
                management::prepare(
                    listeners.management,
                    allowed_origins,
                    routes,
                    lifecycle::wait(receiver.clone()),
                ),
                inference::prepare(
                    listeners.inference,
                    router,
                    lifecycle::wait(receiver.clone()),
                ),
            ),
            (None, None) => (
                idle_until_shutdown(receiver.clone()),
                idle_until_shutdown(receiver.clone()),
            ),
            _ => return Err(ServerError::InconsistentTransports),
        };
        let (private, private_address) = match private {
            Some(listener) => {
                let prepared = private::prepare(
                    listener.configuration,
                    listener.routes,
                    listener.timing,
                    lifecycle::wait(receiver.clone()),
                )
                .await?;
                let address = prepared.address;
                (
                    Box::pin(prepared.serve())
                        as Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>,
                    Some(address),
                )
            }
            None => (
                Box::pin(std::future::pending())
                    as Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>,
                None,
            ),
        };
        let shutdown = Box::pin(async move {
            shutdown.await;
            info!(
                event_kind = "server_shutdown_requested",
                "backend server shutdown requested"
            );
            let _ = sender.send(true);
        });
        Ok(Self {
            management,
            inference,
            private,
            private_address,
            shutdown,
        })
    }

    pub(crate) fn private_address(&self) -> Option<std::net::SocketAddr> {
        self.private_address
    }

    pub(crate) async fn serve(self) -> ServerResult<()> {
        let span = info_span!(
            "backend_dispatch",
            operation = "backend_dispatch",
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let result = lifecycle::serve_transports(self)
            .instrument(span.clone())
            .await;
        span.record("outcome", if result.is_ok() { "success" } else { "failed" });
        match &result {
            Ok(()) => info!(
                event_kind = "server_shutdown_completed",
                "backend server transports stopped"
            ),
            Err(error) => {
                error!(event_kind = "server_shutdown_failed", error = %error, "backend server transport lifecycle failed")
            }
        }
        result
    }
}

/// A transport that serves nothing and ends with the server shutdown.
fn idle_until_shutdown(
    receiver: tokio::sync::watch::Receiver<bool>,
) -> Pin<Box<dyn Future<Output = ServerResult<()>> + Send>> {
    Box::pin(async move {
        lifecycle::wait(receiver).await;
        Ok(())
    })
}

/// Typed server boundary failures.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Listener(#[from] ListenerError),

    #[error("backend HTTP listener failed")]
    Management(#[source] anyhow::Error),

    #[error(transparent)]
    Router(#[from] crate::router::http::ConnectionError),

    #[error("backend private listener failed")]
    Private(#[source] anyhow::Error),

    #[error("the public listeners and the inference router must be supplied together")]
    InconsistentTransports,
}

/// Errors while binding the backend-owned development listeners.
#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    #[error("failed to bind development backend to configured address {address:?}")]
    Backend {
        address: std::net::SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("failed to bind inference router to configured address {address:?}")]
    Inference {
        address: std::net::SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("failed to inspect bound inference router listener")]
    InferenceAddress(#[source] io::Error),

    #[error("failed to inspect bound backend listener")]
    ManagementAddress(#[source] io::Error),

    #[error("failed to bind private backend to configured address {address:?}")]
    Private {
        address: std::net::SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("failed to inspect bound private backend listener")]
    PrivateAddress(#[source] io::Error),
}

/// Result type owned by server transport dispatch.
pub type ServerResult<T> = Result<T, ServerError>;

pub(crate) async fn bind(
    management_address: std::net::SocketAddr,
    inference_address: std::net::SocketAddr,
) -> ServerResult<BoundTransports> {
    Ok(address::bind(management_address, inference_address).await?)
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    #[tokio::test]
    async fn public_listeners_without_a_router_are_rejected() {
        let loopback: std::net::SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
        let transports = super::bind(loopback, loopback)
            .await
            .expect("bound listeners");

        let result = super::Server::prepare(
            transports,
            Vec::new(),
            tonic::service::Routes::default(),
            None,
            None,
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(
            result,
            Err(super::ServerError::InconsistentTransports)
        ));
    }

    #[test]
    fn process_fatal_backend_listener_error_includes_its_cause() {
        let error = super::ServerError::Management(anyhow!("fixture bind failure"));

        assert_eq!(error.to_string(), "backend HTTP listener failed");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            "backend HTTP listener failed: fixture bind failure"
        );
    }

    #[test]
    fn listener_errors_render_their_context_and_cause_once() {
        let error = super::ListenerError::Backend {
            address: "127.0.0.1:50051".parse().expect("listener address"),
            source: std::io::Error::other("injected bind failure"),
        };

        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            "failed to bind development backend to configured address 127.0.0.1:50051: injected bind failure"
        );
    }
}
