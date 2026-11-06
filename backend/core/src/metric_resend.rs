//! Re-sends a hosting node's latest attributed figures as metric records at the start of every
//! wall-clock second.
//!
//! Writers that record what a node's status reports use it, so history steps with each hardware
//! collection and never diverges from the current value. A figure older than the freshness bound
//! records nothing, so history shows a gap rather than a held stale value.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dotcore_domain::clock::Clock;
use dotcore_observability::{MetricRecord, MetricRecorder};

/// Interval at which a node re-sends the figures it attributes.
pub(crate) const RESEND_INTERVAL: Duration = Duration::from_secs(1);

/// Time an observation may take to collect beyond twice the hardware refresh interval before its
/// figure is too old to record.
pub(crate) const COLLECTION_ALLOWANCE: Duration = Duration::from_secs(5);

/// Records what `records` returns for each wall-clock second, at its start, through `recorder`,
/// until dropped. A second the caller missed stays absent, and a second with no records sends
/// nothing.
pub(crate) async fn resend_every_second(
    clock: &dyn Clock,
    recorder: &dyn MetricRecorder,
    mut records: impl FnMut(SystemTime) -> Vec<MetricRecord>,
) {
    let mut last_second = None;
    loop {
        let into_second =
            Duration::from_nanos(u64::from(since_epoch(clock.wall_now()).subsec_nanos()));
        clock
            .sleep_until(clock.now() + (RESEND_INTERVAL - into_second))
            .await;
        let at = clock.wall_now();
        let second = since_epoch(at).as_secs();
        // Woken just before the boundary: wait for it rather than record a second twice.
        if last_second == Some(second) {
            continue;
        }
        last_second = Some(second);

        let records = records(at);
        if !records.is_empty() {
            recorder.record(records);
        }
    }
}

/// Oldest observation a writer still records: twice the hardware refresh interval plus the
/// collection allowance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Freshness {
    max_age: Duration,
}

impl Freshness {
    /// The bound for a node refreshing its hardware observations every `refresh_interval`.
    pub(crate) fn new(refresh_interval: Duration, collection_allowance: Duration) -> Self {
        Self {
            max_age: refresh_interval * 2 + collection_allowance,
        }
    }

    /// Whether a figure observed at `observed_at` is within the bound at `at`. One observed after
    /// `at`, as when the wall clock stepped back, counts as fresh; one without a time never does.
    pub(crate) fn admits(&self, observed_at: Option<SystemTime>, at: SystemTime) -> bool {
        observed_at.is_some_and(|observed_at| {
            at.duration_since(observed_at)
                .map_or(true, |age| age <= self.max_age)
        })
    }
}

fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or_default()
}
