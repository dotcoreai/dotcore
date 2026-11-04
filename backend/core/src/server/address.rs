//! Listener binding and bound-address inspection.

use std::net::SocketAddr;

use tracing::{Instrument, error, field, info, info_span};

use super::ListenerError;

/// Actual addresses assigned to the backend transports; a listener the backend does not serve has
/// no address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerAddresses {
    pub management: Option<SocketAddr>,
    pub inference: Option<SocketAddr>,
    pub private: Option<SocketAddr>,
}

/// Exact open listeners reserved for one prepared backend.
pub(crate) struct BoundTransports {
    pub(super) public: Option<PublicListeners>,
    addresses: ServerAddresses,
}

/// The manager's management and inference listeners, bound together.
pub(super) struct PublicListeners {
    pub(super) management: tokio::net::TcpListener,
    pub(super) inference: tokio::net::TcpListener,
}

impl BoundTransports {
    /// Reserves no public listener, for a node that serves only its private channel.
    pub(crate) fn private_only() -> Self {
        Self {
            public: None,
            addresses: ServerAddresses {
                management: None,
                inference: None,
                private: None,
            },
        }
    }

    pub(crate) fn addresses(&self) -> ServerAddresses {
        self.addresses
    }
}

/// Binds the manager's management and inference listeners.
pub(crate) async fn bind(
    management_address: SocketAddr,
    inference_address: SocketAddr,
) -> Result<BoundTransports, ListenerError> {
    let management = bind_listener(management_address, false).await?;
    let inference = bind_listener(inference_address, true).await?;
    let addresses = ServerAddresses {
        management: Some(
            management
                .local_addr()
                .map_err(ListenerError::ManagementAddress)?,
        ),
        inference: Some(
            inference
                .local_addr()
                .map_err(ListenerError::InferenceAddress)?,
        ),
        private: None,
    };
    info!(event_kind = "server_listeners_bound", management_address = ?addresses.management, inference_address = ?addresses.inference, "backend listeners bound");
    Ok(BoundTransports {
        public: Some(PublicListeners {
            management,
            inference,
        }),
        addresses,
    })
}

async fn bind_listener(
    address: SocketAddr,
    inference: bool,
) -> Result<tokio::net::TcpListener, ListenerError> {
    let operation = if inference {
        "inference_listener_bind"
    } else {
        "backend_listener_bind"
    };
    let span = info_span!("listener_bind", operation, %address, outcome = field::Empty);
    let result = tokio::net::TcpListener::bind(address)
        .instrument(span.clone())
        .await;
    span.record("outcome", if result.is_ok() { "success" } else { "failed" });
    if let Err(source) = &result {
        error!(event_kind = "server_listener_bind_failed", operation, address = %address, error = %source, "backend listener bind failed");
    } else {
        info!(event_kind = "server_listener_bound", operation, address = %address, "backend listener bound");
    }
    result.map_err(|source| {
        if inference {
            ListenerError::Inference { address, source }
        } else {
            ListenerError::Backend { address, source }
        }
    })
}
