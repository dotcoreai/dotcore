//! Shared pieces of the loopback readiness probes that each engine runs against its own service.
//!
//! An engine supplies only its readiness check; this module ends the check early when the probe
//! is cancelled, turns a ready or not-ready answer into readiness evidence, and classifies
//! socket failures. Refused, reset, or timed-out connections mean the engine is still starting
//! and the probe is retried; any other failure is terminal.

use std::future::Future;

use crate::inference::{EngineProbeError, EngineReadiness, InferenceCancellation, InferenceFuture};

/// Runs one engine readiness check, ending with a terminal error when the probe is cancelled.
pub(super) fn probe_readiness<'a>(
    mut cancellation: InferenceCancellation,
    served_model_id: &'a str,
    check: impl Future<Output = Result<bool, EngineProbeError>> + Send + 'a,
) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
    Box::pin(async move {
        tokio::select! {
            reason = cancellation.cancelled() => Err(EngineProbeError::Terminal(anyhow::anyhow!(
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

/// Wraps a readiness failure that retrying cannot fix.
pub(super) fn terminal(error: impl Into<anyhow::Error>) -> EngineProbeError {
    EngineProbeError::Terminal(error.into())
}

/// Classifies a socket failure: a not-yet-listening or restarting engine is transient.
pub(super) fn io_error(context: &str, error: std::io::Error) -> EngineProbeError {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::TimedOut => EngineProbeError::Transient,
        _ => terminal(anyhow::Error::new(error).context(context.to_owned())),
    }
}
