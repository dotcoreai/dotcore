//! Binds one Strata loopback service to the engine-neutral inference contract.
//!
//! The binding performs exactly one transport attempt through the shared OpenAI-compatible
//! loopback transport and preserves cancellation, fallback disposition, streaming, and
//! passthrough response evidence for the router.
//! Strata's refusals pass through unchanged; none is classified as an invalid request.

use std::sync::Arc;

use crate::inference::{EngineInvoker, EngineTransportFactory};
use crate::model::engine::loopback_http::{LoopbackInvoker, RejectionClassifiers};

/// Creates endpoint-bound Strata transport invokers for engine bindings.
pub(crate) struct StrataTransportFactory;

impl EngineTransportFactory for StrataTransportFactory {
    fn bind_loopback(&self, port: u16, served_model_id: String) -> Arc<dyn EngineInvoker> {
        LoopbackInvoker::bind(
            port,
            served_model_id,
            super::REQUEST_KINDS,
            RejectionClassifiers::default(),
        )
    }
}
