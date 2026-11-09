//! Test support for metric writers, compiled for this crate's unit tests and, through the
//! `test-support` feature, for the tests of the crates above it.
//!
//! A capturing recorder keeps every batch a writer hands it, in order. Tests read the records
//! back, take the batches one at a time, or wait until a record they expect arrives, always under
//! a deadline instead of a fixed sleep.

use std::{sync::Mutex, time::Duration};

use tokio::sync::watch;

use crate::{record::MetricRecord, store::MetricRecorder};

/// How long a test waits for a writer to record before it fails.
const RECORD_DEADLINE: Duration = Duration::from_secs(10);

/// Metric recorder keeping every batch it is handed, announcing each one.
#[derive(Default)]
pub struct CapturingRecorder {
    captured: Mutex<Captured>,
    batches: watch::Sender<usize>,
}

impl CapturingRecorder {
    /// Every record handed so far, in recording order.
    pub fn records(&self) -> Vec<MetricRecord> {
        self.captured
            .lock()
            .expect("captured records")
            .batches
            .concat()
    }

    /// The oldest batch not taken yet, if one was recorded.
    pub fn try_next_batch(&self) -> Option<Vec<MetricRecord>> {
        let mut captured = self.captured.lock().expect("captured records");
        let batch = captured.batches.get(captured.taken).cloned()?;
        captured.taken += 1;
        Some(batch)
    }

    /// The oldest batch not taken yet, waiting for the writer to record one. Panics when none
    /// arrives within the deadline.
    pub async fn next_batch(&self) -> Vec<MetricRecord> {
        self.wait_until(|| self.try_next_batch())
            .await
            .expect("writer records a batch before the deadline")
    }

    /// The first answer `find` gives over every record handed so far, waiting for new batches
    /// until it gives one. `None` when no batch within the deadline produced an answer.
    pub async fn wait_for<T>(&self, find: impl Fn(&[MetricRecord]) -> Option<T>) -> Option<T> {
        self.wait_until(|| find(&self.records())).await
    }

    async fn wait_until<T>(&self, mut check: impl FnMut() -> Option<T>) -> Option<T> {
        let mut batches = self.batches.subscribe();
        dotcore_domain::real_time::bound(RECORD_DEADLINE, async {
            loop {
                if let Some(answer) = check() {
                    return answer;
                }
                batches
                    .changed()
                    .await
                    .expect("the recorder outlives its waiters");
            }
        })
        .await
        .ok()
    }
}

impl MetricRecorder for CapturingRecorder {
    fn record(&self, records: Vec<MetricRecord>) {
        self.captured
            .lock()
            .expect("captured records")
            .batches
            .push(records);
        self.batches.send_modify(|count| *count += 1);
    }
}

/// Batches handed to a capturing recorder and how many a test has taken.
#[derive(Default)]
struct Captured {
    batches: Vec<Vec<MetricRecord>>,
    taken: usize,
}
