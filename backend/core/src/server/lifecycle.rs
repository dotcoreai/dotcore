//! Coordinated management and inference transport shutdown.

use tracing::{Instrument, Span};

use super::{Server, ServerError, ServerResult};

pub(super) async fn serve_transports(server: Server) -> ServerResult<()> {
    let Server {
        management,
        inference,
        private,
        private_address: _,
        shutdown,
    } = server;
    let shutdown_task = tokio::spawn(shutdown.instrument(Span::current()));
    let mut inference = tokio::spawn(inference.instrument(Span::current()));
    let mut private = tokio::spawn(private.instrument(Span::current()));
    tokio::pin!(management);
    let result = tokio::select! {
        result = &mut management => result,
        result = &mut inference => result.map_err(|error| ServerError::Management(anyhow::Error::new(error)))?,
        result = &mut private => result.map_err(|error| ServerError::Private(anyhow::Error::new(error)))?,
    };
    inference.abort();
    private.abort();
    shutdown_task.abort();
    result
}

pub(super) async fn wait(mut receiver: tokio::sync::watch::Receiver<bool>) {
    if !*receiver.borrow() {
        let _ = receiver.changed().await;
    }
}
