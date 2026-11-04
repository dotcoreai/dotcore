//! Reports the unexpected end of a long-lived background loop, such as a node view poller, in the
//! backend log.
//!
//! These loops never return on their own, so a return or a panic is always a fault. Without this
//! wrapper a panic reaches only the process stderr and the loop stops silently while the process
//! keeps running.

use std::{any::Any, future::Future, panic::AssertUnwindSafe};

use futures_util::FutureExt as _;

/// Runs one long-lived background loop named `loop_name` and logs its unexpected end.
///
/// Such loops never return on their own, so their task ends only by abort, return or panic. A
/// panic otherwise reaches only the process stderr, never the site log, and the loop stops
/// silently while the process keeps running.
pub async fn log_unexpected_loop_end(
    loop_name: &'static str,
    background_loop: impl Future<Output = ()>,
) {
    match AssertUnwindSafe(background_loop).catch_unwind().await {
        Ok(()) => tracing::error!(
            event_kind = "background_loop_ended",
            loop_name,
            outcome = "returned",
            "Background loop ended unexpectedly"
        ),
        Err(panic) => tracing::error!(
            event_kind = "background_loop_ended",
            loop_name,
            outcome = "panicked",
            panic = panic_message(panic.as_ref()),
            "Background loop panicked"
        ),
    }
}

/// Text of a panic payload, which is a string for every `panic!` with a message.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}
