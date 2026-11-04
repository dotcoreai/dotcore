//! gRPC-Web management transport assembly.

use std::{future::Future, pin::Pin};

use http::HeaderValue;
use tonic::transport::{Server, server::TcpIncoming};
use tonic_web::GrpcWebLayer;
use tracing::{Instrument, field, info_span};

use super::{ServerError, ServerResult, cors};

pub(super) fn prepare<F>(
    listener: tokio::net::TcpListener,
    allowed_origins: Vec<HeaderValue>,
    routes: tonic::service::Routes,
    shutdown: F,
) -> Pin<Box<dyn Future<Output = ServerResult<()>> + Send>>
where
    F: Future<Output = ()> + Send + 'static,
{
    let span = info_span!(
        "backend_listener",
        operation = "backend_listener",
        transport = "grpc_web",
        outcome = field::Empty
    );
    let cors = (!allowed_origins.is_empty()).then(|| cors::for_origins(allowed_origins));
    let server = Server::builder()
        .accept_http1(true)
        .layer(tower::util::option_layer(cors))
        .layer(GrpcWebLayer::new())
        .add_routes(routes);
    Box::pin(async move {
        let result = server
            .serve_with_incoming_shutdown(incoming(listener), shutdown)
            .instrument(span.clone())
            .await;
        span.record("outcome", if result.is_ok() { "stopped" } else { "failed" });
        result.map_err(|error| ServerError::Management(anyhow::Error::new(error)))
    })
}

// The server ignores its own TCP options for a supplied listener, so its sockets set no delay
// here: with the delay on, a gRPC-Web response's trailer waits for the client's delayed
// acknowledgment of the body.
fn incoming(listener: tokio::net::TcpListener) -> TcpIncoming {
    TcpIncoming::from(listener).with_nodelay(Some(true))
}
