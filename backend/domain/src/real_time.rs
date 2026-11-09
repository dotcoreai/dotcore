//! Real-time waits and wall-time reads for real I/O, real processes and real records.
//!
//! Behavior timing waits through the backend clock, so a test clock's advance reaches it. A few
//! waits must follow real time whatever clock the backend runs: the bound on a call to another
//! process, a daemon or the network, and the pause between two polls of a real file or process.
//! Likewise a few wall-time reads record when something really happened. Production code reaches
//! real-time timers and the host's wall time through this module; only the clocks themselves and
//! the launcher, which runs below the clock, read them directly.
#![expect(
    clippy::disallowed_methods,
    reason = "real time is only for real I/O, real processes and records of real wall time"
)]

use std::{
    future::Future,
    time::{Duration, SystemTime},
};

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

/// Reads the host's wall time, for a record of when something really happened: a reading of a
/// real process, a persisted timestamp or a unique identifier.
pub fn wall_now() -> SystemTime {
    SystemTime::now()
}

/// Pauses for `duration` of real time: between two polls of a real file or process, or while a
/// real connection winds down.
pub async fn pause(duration: Duration) {
    tokio::time::sleep(duration).await;
}
