//! Aggregates model artifact transfer observations and preparation stages for one invocation.
//!
//! Transfer adapters update independent artifact slots without asynchronous locks. Callers receive
//! deployment-level byte snapshots only after every artifact has been classified, while stage
//! observers report verification, publication, and reuse separately.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

/// Cumulative transfer progress across the model artifacts selected by one launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelArtifactProgress {
    pub completed: u64,
    pub total: Option<u64>,
}

/// Artifact preparation stage without byte progress. Each value marks the work currently underway
/// or a verified local reuse decision so the caller can report it in deployment status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelArtifactStage {
    Verify,

    Publish,

    Reuse,
}

impl ModelArtifactStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Publish => "publish",
            Self::Reuse => "reuse",
        }
    }
}

pub type ModelArtifactStageObserver = Arc<dyn Fn(ModelArtifactStage) + Send + Sync>;
pub type ModelArtifactProgressObserver = Arc<dyn Fn(ModelArtifactProgress) + Send + Sync>;

#[derive(Clone)]
pub(super) struct ModelArtifactProgressTracker {
    inner: Arc<TrackerInner>,
}

impl ModelArtifactProgressTracker {
    pub(super) fn new(artifact_count: usize, observer: ModelArtifactProgressObserver) -> Self {
        Self {
            inner: Arc::new(TrackerInner {
                slots: (0..artifact_count).map(|_| TransferSlot::new()).collect(),
                observer,
            }),
        }
    }

    pub(super) fn slot(&self, index: usize) -> ModelArtifactProgressSlot {
        assert!(index < self.inner.slots.len(), "artifact progress slot");
        ModelArtifactProgressSlot {
            tracker: self.clone(),
            index,
        }
    }

    fn publish(&self) {
        let mut completed = 0_u64;
        let mut total = 0_u64;
        let mut has_transfer = false;
        let mut totals_known = true;

        for slot in &self.inner.slots {
            match slot.classification.load(Ordering::SeqCst) {
                PENDING => return,
                TRANSFERRING => {
                    has_transfer = true;
                    let carried = slot.carried.load(Ordering::SeqCst);
                    let transfer_completed = slot.in_flight_completed();
                    completed = completed
                        .saturating_add(carried)
                        .saturating_add(transfer_completed);
                    // A file the hub reuses from an identical cached blob starts no transfer.
                    let transfer_total = slot.total.load(Ordering::SeqCst);
                    let in_flight = slot.in_flight.load(Ordering::SeqCst);
                    if (in_flight && (transfer_total == 0 || transfer_completed > transfer_total))
                        || slot.remaining_files.load(Ordering::SeqCst) > 0
                    {
                        totals_known = false;
                    } else {
                        total = total.saturating_add(carried).saturating_add(transfer_total);
                    }
                }
                CACHE_SATISFIED | FAILED => {}
                _ => unreachable!("artifact progress classification"),
            }
        }

        if has_transfer {
            (self.inner.observer)(ModelArtifactProgress {
                completed,
                total: totals_known.then_some(total),
            });
        }
    }
}

#[derive(Clone)]
pub(super) struct ModelArtifactProgressSlot {
    tracker: ModelArtifactProgressTracker,
    index: usize,
}

impl ModelArtifactProgressSlot {
    pub(super) fn cache_satisfied(&self) {
        self.slot()
            .classification
            .compare_exchange(PENDING, CACHE_SATISFIED, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
        self.tracker.publish();
    }

    /// Starts the next file of a multi-file artifact: bytes of the file transferred before stay
    /// counted, and the artifact total stays unknown while `remaining_files` more files may follow.
    pub(super) fn file_started(&self, remaining_files: u64) {
        let slot = self.slot();
        if slot.classification.load(Ordering::SeqCst) == TRANSFERRING {
            slot.carried
                .fetch_add(slot.in_flight_completed(), Ordering::SeqCst);
            slot.ordinary_completed.store(0, Ordering::SeqCst);
            slot.aggregate_completed.store(0, Ordering::SeqCst);
            slot.total.store(0, Ordering::SeqCst);
        }
        slot.in_flight.store(false, Ordering::SeqCst);
        slot.remaining_files
            .store(remaining_files, Ordering::SeqCst);
        self.tracker.publish();
    }

    pub(super) fn transfer_started(&self, total: u64) {
        let slot = self.slot();
        slot.ordinary_completed.store(0, Ordering::SeqCst);
        slot.aggregate_completed.store(0, Ordering::SeqCst);
        slot.total.store(total, Ordering::SeqCst);
        slot.in_flight.store(true, Ordering::SeqCst);
        slot.classification.store(TRANSFERRING, Ordering::SeqCst);
        self.tracker.publish();
    }

    pub(super) fn ordinary_progress(&self, completed: u64, total: u64) {
        self.update_progress(&self.slot().ordinary_completed, completed, total);
    }

    pub(super) fn aggregate_progress(&self, completed: u64, total: u64) {
        self.update_progress(&self.slot().aggregate_completed, completed, total);
    }

    pub(super) fn transfer_completed(&self) {
        let slot = self.slot();
        let total = slot.total.load(Ordering::SeqCst);
        if total > 0 {
            slot.ordinary_completed.fetch_max(total, Ordering::SeqCst);
        }
        self.tracker.publish();
    }

    pub(super) fn failed(&self) {
        self.slot()
            .classification
            .compare_exchange(PENDING, FAILED, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
        self.tracker.publish();
    }

    fn update_progress(&self, channel: &AtomicU64, completed: u64, total: u64) {
        let slot = self.slot();
        slot.in_flight.store(true, Ordering::SeqCst);
        slot.classification.store(TRANSFERRING, Ordering::SeqCst);
        if total > 0 {
            slot.total.fetch_max(total, Ordering::SeqCst);
        }
        channel.fetch_max(completed, Ordering::SeqCst);
        self.tracker.publish();
    }

    fn slot(&self) -> &TransferSlot {
        &self.tracker.inner.slots[self.index]
    }
}

struct TrackerInner {
    slots: Vec<TransferSlot>,
    observer: ModelArtifactProgressObserver,
}

/// Transfer state of one artifact. `carried` holds bytes of earlier files of a multi-file
/// artifact; the other counters describe the file in flight, if `in_flight` is set.
struct TransferSlot {
    classification: AtomicU8,
    carried: AtomicU64,
    remaining_files: AtomicU64,
    in_flight: AtomicBool,
    ordinary_completed: AtomicU64,
    aggregate_completed: AtomicU64,
    total: AtomicU64,
}

impl TransferSlot {
    fn new() -> Self {
        Self {
            classification: AtomicU8::new(PENDING),
            carried: AtomicU64::new(0),
            remaining_files: AtomicU64::new(0),
            in_flight: AtomicBool::new(false),
            ordinary_completed: AtomicU64::new(0),
            aggregate_completed: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    /// Returns the greater of the ordinary and Xet byte counts of the file in flight.
    fn in_flight_completed(&self) -> u64 {
        self.ordinary_completed
            .load(Ordering::SeqCst)
            .max(self.aggregate_completed.load(Ordering::SeqCst))
    }
}

const PENDING: u8 = 0;
const CACHE_SATISFIED: u8 = 1;
const TRANSFERRING: u8 = 2;
const FAILED: u8 = 3;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn aggregates_known_concurrent_transfers_after_every_artifact_is_classified() {
        let (tracker, snapshots) = tracker(2);
        let first = tracker.slot(0);
        let second = tracker.slot(1);

        first.transfer_started(100);
        first.ordinary_progress(25, 100);
        assert_eq!(snapshots.lock().expect("snapshots").as_slice(), []);

        second.transfer_started(300);
        second.aggregate_progress(120, 300);

        assert_eq!(
            snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 145,
                total: Some(400),
            })
        );
    }

    #[test]
    fn omits_total_while_any_transfer_total_is_unknown() {
        let (tracker, snapshots) = tracker(2);
        let known = tracker.slot(0);
        let unknown = tracker.slot(1);

        known.transfer_started(100);
        known.ordinary_progress(50, 100);
        unknown.transfer_started(0);
        unknown.ordinary_progress(20, 0);

        assert_eq!(
            snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 70,
                total: None,
            })
        );
    }

    #[test]
    fn cache_hits_add_no_bytes_and_all_cached_launches_emit_nothing() {
        let (mixed, mixed_snapshots) = tracker(2);
        mixed.slot(0).transfer_started(100);
        mixed.slot(0).ordinary_progress(40, 100);
        mixed.slot(1).cache_satisfied();

        assert_eq!(
            mixed_snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 40,
                total: Some(100),
            })
        );

        let (cached, cached_snapshots) = tracker(2);
        cached.slot(0).cache_satisfied();
        cached.slot(1).cache_satisfied();

        assert!(cached_snapshots.lock().expect("snapshots").is_empty());
    }

    #[test]
    fn uses_the_greater_ordinary_or_xet_snapshot_without_double_counting() {
        let (tracker, snapshots) = tracker(1);
        let slot = tracker.slot(0);

        slot.transfer_started(100);
        slot.ordinary_progress(60, 100);
        slot.aggregate_progress(45, 100);
        assert_eq!(
            snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 60,
                total: Some(100),
            })
        );

        slot.aggregate_progress(80, 100);
        assert_eq!(
            snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 80,
                total: Some(100),
            })
        );
    }

    #[test]
    fn repeated_transfer_start_resets_the_artifact_snapshot() {
        let (tracker, snapshots) = tracker(1);
        let slot = tracker.slot(0);

        slot.transfer_started(100);
        slot.ordinary_progress(100, 100);
        slot.transfer_completed();
        slot.transfer_started(200);

        assert_eq!(
            snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 0,
                total: Some(200),
            })
        );
    }

    #[test]
    fn accumulates_multi_file_bytes_and_reports_the_total_after_the_last_file_starts() {
        let (tracker, snapshots) = tracker(1);
        let slot = tracker.slot(0);
        let last = |snapshots: &Mutex<Vec<ModelArtifactProgress>>| {
            snapshots.lock().expect("snapshots").last().cloned()
        };

        slot.file_started(2);
        slot.transfer_started(100);
        slot.ordinary_progress(100, 100);
        slot.transfer_completed();
        assert_eq!(
            last(&snapshots),
            Some(ModelArtifactProgress {
                completed: 100,
                total: None,
            })
        );

        slot.file_started(1);
        slot.transfer_started(200);
        slot.aggregate_progress(150, 200);
        assert_eq!(
            last(&snapshots),
            Some(ModelArtifactProgress {
                completed: 250,
                total: None,
            })
        );

        slot.transfer_completed();
        slot.file_started(0);
        slot.transfer_started(50);
        slot.ordinary_progress(10, 50);
        assert_eq!(
            last(&snapshots),
            Some(ModelArtifactProgress {
                completed: 310,
                total: Some(350),
            })
        );

        slot.ordinary_progress(50, 50);
        slot.transfer_completed();
        slot.file_started(0);
        assert_eq!(
            last(&snapshots),
            Some(ModelArtifactProgress {
                completed: 350,
                total: Some(350),
            })
        );

        let completed = snapshots
            .lock()
            .expect("snapshots")
            .iter()
            .map(|progress| progress.completed)
            .collect::<Vec<_>>();
        assert!(
            completed.windows(2).all(|pair| pair[0] <= pair[1]),
            "{completed:?}"
        );
    }

    #[test]
    fn classifies_a_failed_sibling_without_inventing_transfer_bytes() {
        let (tracker, snapshots) = tracker(2);
        let active = tracker.slot(0);

        active.transfer_started(100);
        active.ordinary_progress(30, 100);
        tracker.slot(1).failed();

        assert_eq!(
            snapshots.lock().expect("snapshots").last(),
            Some(&ModelArtifactProgress {
                completed: 30,
                total: Some(100),
            })
        );
    }

    fn tracker(
        artifact_count: usize,
    ) -> (
        ModelArtifactProgressTracker,
        Arc<Mutex<Vec<ModelArtifactProgress>>>,
    ) {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let observed = snapshots.clone();
        let tracker = ModelArtifactProgressTracker::new(
            artifact_count,
            Arc::new(move |progress| {
                observed.lock().expect("snapshots").push(progress);
            }),
        );
        (tracker, snapshots)
    }
}
