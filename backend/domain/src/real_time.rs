//! Real-time waits for real I/O and real processes.
//!
//! Behavior timing waits through the backend clock, so a test clock's advance reaches it. A few
//! waits must follow real time whatever clock the backend runs: the bound on a call to another
//! process, a daemon or the network, and the pause between two polls of a real file or process.
//! This module is the only place production code reaches real-time timers for them.
#![expect(
    clippy::disallowed_methods,
    reason = "real time is only for bounding real I/O and pacing polls of real files and processes"
)]

use std::{future::Future, time::Duration};

use tokio::time::{Instant, Sleep, error::Elapsed};

/// Runs `io` until it completes or `limit` of real time passes, whichever is first.
pub async fn bound<F: Future>(limit: Duration, io: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(limit, io).await
}

/// Runs `io` until it completes or real time reaches `deadline`, whichever is first.
pub async fn bound_until<F: Future>(deadline: Instant, io: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout_at(deadline, io).await
}

/// Completes when real time reaches `at`: the end a real connection or request is held to. The
/// returned timer can be reset while the connection stays in use.
pub fn deadline(at: Instant) -> Sleep {
    tokio::time::sleep_until(at)
}

/// Pauses for `interval` of real time between two polls of a real file or process.
pub async fn poll_pause(interval: Duration) {
    tokio::time::sleep(interval).await;
}
