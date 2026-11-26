//! Shared pieces of the loopback readiness probes that each engine runs against its own service.
//!
//! An engine supplies only its readiness check; this module ends the check early when the probe
//! is cancelled, turns a ready or not-ready answer into readiness evidence, and classifies
//! socket failures. Every failure is retried as the engine may still be starting; only an engine
//! that reports it can never serve the deployment fails it.

use std::future::Future;

use crate::inference::{EngineProbeError, EngineReadiness, InferenceCancellation, InferenceFuture};

/// Runs one engine readiness check, ending with an unexpected error when the probe is cancelled.
pub(super) fn probe_readiness<'a>(
    mut cancellation: InferenceCancellation,
    served_model_id: &'a str,
    check: impl Future<Output = Result<bool, EngineProbeError>> + Send + 'a,
) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
    Box::pin(async move {
        tokio::select! {
            reason = cancellation.cancelled() => Err(EngineProbeError::Unexpected(anyhow::anyhow!(
                "engine readiness probe cancelled: {reason}"
            ))),
            ready = check => Ok(if ready? {
                EngineReadiness::Ready {
                    served_model_id: served_model_id.to_owned(),
                }
            } else {
                EngineReadiness::Loading
            }),
        }
    })
}

/// Wraps a readiness failure that is retried, since the engine may still become ready.
pub(super) fn unexpected(error: impl Into<anyhow::Error>) -> EngineProbeError {
    EngineProbeError::Unexpected(error.into())
}

/// Classifies a socket failure: a not-yet-listening or restarting engine is transient, any other is unexpected.
pub(super) fn io_error(context: &str, error: std::io::Error) -> EngineProbeError {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::TimedOut => EngineProbeError::Transient,
        _ => unexpected(anyhow::Error::new(error).context(context.to_owned())),
    }
}
