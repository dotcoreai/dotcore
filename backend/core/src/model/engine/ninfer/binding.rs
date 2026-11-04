//! Binds one ninfer loopback service to the engine-neutral inference contract.
//!
//! The binding performs exactly one transport attempt and preserves cancellation,
//! fallback disposition, streaming, and passthrough response evidence for the router.

use std::sync::Arc;

use crate::inference::{EngineInvoker, EngineTransportFactory};
use crate::model::engine::loopback_http::LoopbackInvoker;

/// Creates endpoint-bound ninfer transport invokers for engine bindings.
pub(crate) struct NinferTransportFactory;

impl EngineTransportFactory for NinferTransportFactory {
    fn bind_loopback(&self, port: u16, served_model_id: String) -> Arc<dyn EngineInvoker> {
        LoopbackInvoker::bind(port, served_model_id, super::REQUEST_KINDS, None)
    }
}
