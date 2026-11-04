//! Samples a node's hardware memory at the start of every wall-clock second and records it as
//! metrics, so memory history holds one point per second between the slower full snapshots.
//!
//! A second the sampler could not sample in, because the previous sample overran, failed or read
//! nothing, stays absent and is recorded as skipped. A backward wall-clock step pauses sampling until
//! the clock passes the last sampled second again, so no second is sampled twice; a step back
//! further than the node buffers metrics starts over instead of pausing that long.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_domain::{
    clock::{Clock, next_boundary},
    hardware_observation::HardwareSnapshotSource,
};
use dotcore_node_hardware::LocalNodeCapability;
use dotcore_observability::MetricRecorder;
use tracing::{debug, warn};

use crate::node::metrics::{
    HARDWARE_SAMPLE_INTERVAL, hardware_sample_records, hardware_sampling_skipped_record,
};

/// Longest a backward wall-clock step pauses sampling: beyond it, the node's metric forwarder has
/// already let go of the seconds the clock went back to, so sampling starts over.
const MAX_BACKWARD_PAUSE: Duration = Duration::from_secs(15 * 60);

/// Samples one node's hardware memory every wall-clock second into a metric recorder.
pub(crate) struct NodeSampler<C> {
    node_id: String,
    hardware: Arc<LocalNodeCapability<C>>,
    recorder: Arc<dyn MetricRecorder>,
    clock: Arc<dyn Clock>,
}

impl<C> NodeSampler<C>
where
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    /// Creates a sampler of SiteNode `node_id` reading `hardware`, recording through `recorder`,
    /// and scheduling on `clock`.
    pub(crate) fn new(
        node_id: &str,
        hardware: Arc<LocalNodeCapability<C>>,
        recorder: Arc<dyn MetricRecorder>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            node_id: node_id.to_owned(),
            hardware,
            recorder,
            clock,
        }
    }

    /// Samples at the start of every wall-clock second until dropped.
    pub(crate) async fn run(self) {
        let mut last_second: Option<u64> = None;
        let mut paused = false;
        loop {
            let wall = self.clock.wall_now();
            let wait = next_boundary(wall, HARDWARE_SAMPLE_INTERVAL)
                .duration_since(wall)
                .unwrap_or_default();
            self.clock.sleep_until(self.clock.now() + wait).await;
            let observed_at = self.clock.wall_now();
            let second = since_epoch(observed_at).as_secs();
            match last_second {
                Some(last) if second + MAX_BACKWARD_PAUSE.as_secs() < last => {
                    warn!(
                        event_kind = "node_hardware_sample",
                        outcome = "clock_reset",
                        behind_seconds = last - second,
                        "wall clock set back beyond the sampling pause bound; sampling starts over"
                    );
                }
                // Woken just before the boundary, or the wall clock stepped back: wait until it
                // passes the last sampled second.
                Some(last) if second <= last => {
                    if second < last && !paused {
                        warn!(
                            event_kind = "node_hardware_sample",
                            outcome = "clock_stepped_back",
                            behind_seconds = last - second,
                            "wall clock stepped back; sampling pauses until it catches up"
                        );
                    }
                    paused |= second < last;
                    continue;
                }
                Some(last) => self.skip(second - last - 1, observed_at),
                None => {}
            }
            paused = false;
            last_second = Some(second);

            match self.hardware.sample().await {
                Ok(sample) => {
                    let records = hardware_sample_records(&self.node_id, &sample, observed_at);
                    if records.is_empty() {
                        self.skip(1, observed_at);
                    } else {
                        self.recorder.record(records);
                    }
                }
                Err(error) => {
                    debug!(
                        event_kind = "node_hardware_sample",
                        outcome = "failed",
                        error = %error,
                        "node hardware sample failed"
                    );
                    self.skip(1, observed_at);
                }
            }
        }
    }

    /// Records `seconds` without a sample, noticed at `observed_at`.
    fn skip(&self, seconds: u64, observed_at: SystemTime) {
        if seconds == 0 {
            return;
        }
        self.recorder.record(vec![hardware_sampling_skipped_record(
            &self.node_id,
            seconds,
            observed_at,
        )]);
        debug!(
            event_kind = "node_hardware_sample",
            outcome = "skipped",
            seconds,
            "node hardware sampling skipped seconds"
        );
    }
}

fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    };

    use dotcore_domain::{
        clock::VirtualClock,
        hardware_observation::{HardwareSample, HostMemorySample, HostSample},
        test_support::tracing::{Record, capture_records_async},
    };
    use dotcore_observability::{MetricRecord, labels::LabelSet};
    use dotcore_proto::dotcore::observation::v1::HardwareSnapshot;

    use super::*;
    use crate::node::metrics::{
        HARDWARE_SAMPLING_SKIPPED, HOST_MEMORY_AVAILABLE, HOST_MEMORY_TOTAL,
    };

    #[tokio::test]
    async fn one_sample_is_recorded_at_the_start_of_every_wall_second() {
        let harness = Harness::start(None);

        for second in 1..=3 {
            harness.clock.advance_to_next_wake().await;
            assert_eq!(
                harness.next_records().await,
                memory_records(second, second as u64)
            );
        }
    }

    #[tokio::test]
    async fn a_second_missed_by_a_slow_sample_stays_absent_and_is_counted() {
        let (entered, mut entered_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_receiver) = mpsc::channel();
        let harness = Harness::start(Some(Gate {
            sample: 2,
            entered,
            release: Mutex::new(release_receiver),
        }));
        harness.clock.advance_to_next_wake().await;
        assert_eq!(harness.next_records().await, memory_records(1, 1));

        // The second sample starts at second 2 and only returns at 3.5, past the next boundary.
        harness.clock.advance_to_next_wake().await;
        Harness::wait(&mut entered_receiver).await;
        harness.clock.advance(Duration::from_millis(1_500));
        release.send(()).expect("release the sample");
        assert_eq!(harness.next_records().await, memory_records(2, 2));
        harness.clock.advance_to_next_wake().await;

        assert_eq!(harness.next_records().await, vec![skipped_record(4, 1.0)]);
        assert_eq!(harness.next_records().await, memory_records(4, 3));
    }

    #[tokio::test]
    async fn an_empty_sample_counts_as_a_skipped_second() {
        let harness = Harness::start_with(None, Some(2));
        harness.clock.advance_to_next_wake().await;
        assert_eq!(harness.next_records().await, memory_records(1, 1));

        harness.clock.advance_to_next_wake().await;
        harness.clock.advance_to_next_wake().await;

        assert_eq!(harness.next_records().await, vec![skipped_record(2, 1.0)]);
        assert_eq!(harness.next_records().await, memory_records(3, 3));
    }

    #[tokio::test]
    async fn a_backward_step_pauses_sampling_and_a_large_one_starts_over() {
        let (harness, records, _) = capture_records_async(|| async {
            let harness = Harness::start(None);
            harness.clock.advance_to_next_wake().await;
            assert_eq!(harness.next_records().await, memory_records(1, 1));

            // Back 2 seconds: seconds 0 and 1 pass without sampling, then sampling resumes at 2.
            harness.clock.step_wall_back(Duration::from_secs(2));
            for _ in 0..3 {
                harness.clock.advance_to_next_wake().await;
            }
            assert_eq!(harness.next_records().await, memory_records(2, 2));

            // Back 20 minutes, beyond the pause bound: the next second is sampled at once.
            harness.clock.step_wall_back(Duration::from_secs(20 * 60));
            harness.clock.advance_to_next_wake().await;
            assert_eq!(harness.next_records().await, memory_records(3 - 20 * 60, 3));
            harness
        })
        .await;

        assert!(harness.records.lock().await.is_empty());
        let outcomes: Vec<&str> = records
            .iter()
            .filter_map(|record| match record {
                Record::Event { level, fields } if level == "WARN" => {
                    fields.get("outcome").map(String::as_str)
                }
                _ => None,
            })
            .collect();
        // Captured fields hold their debug form.
        assert_eq!(outcomes, ["\"clock_stepped_back\"", "\"clock_reset\""]);
    }

    /// Whole seconds after the Unix epoch the virtual wall clock counts from.
    const EPOCH_SECONDS: u64 = 1_700_000_000;

    /// A sampler over fake hardware, recording into a channel, on a virtual clock that starts a
    /// quarter second into second 0.
    struct Harness {
        clock: VirtualClock,
        records: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<MetricRecord>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        fn start(gate: Option<Gate>) -> Self {
            Self::start_with(gate, None)
        }

        /// Starts a sampler whose `empty`th sample reads nothing.
        fn start_with(gate: Option<Gate>, empty: Option<u64>) -> Self {
            let clock = VirtualClock::starting_at(
                UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS) + Duration::from_millis(250),
            );
            let (sender, records) = tokio::sync::mpsc::unbounded_channel();
            let sampler = NodeSampler::new(
                "node-a",
                Arc::new(LocalNodeCapability::new(CountingHardware {
                    samples: AtomicU64::new(0),
                    gate,
                    empty,
                })),
                Arc::new(ChannelRecorder(sender)),
                Arc::new(clock.clone()),
            );
            Self {
                clock,
                records: tokio::sync::Mutex::new(records),
                _task: tokio::spawn(sampler.run()),
            }
        }

        async fn next_records(&self) -> Vec<MetricRecord> {
            Self::wait(&mut *self.records.lock().await).await
        }

        async fn wait<T>(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
            tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                .await
                .expect("sampler progress deadline")
                .expect("sampler running")
        }
    }

    /// Records the sampler made at second `second`, whose sample was the `sample`th.
    fn memory_records(second: i64, sample: u64) -> Vec<MetricRecord> {
        let at = at_second(second);
        let node = LabelSet::new().with("node", "node-a");
        vec![
            MetricRecord::gauge(HOST_MEMORY_TOTAL, node.clone(), at, (sample * 100) as f64),
            MetricRecord::gauge(HOST_MEMORY_AVAILABLE, node, at, sample as f64),
        ]
    }

    /// The skip record of `seconds` noticed at second `second`.
    fn skipped_record(second: i64, seconds: f64) -> MetricRecord {
        MetricRecord::counter(
            HARDWARE_SAMPLING_SKIPPED,
            LabelSet::new().with("node", "node-a"),
            at_second(second),
            seconds,
        )
    }

    fn at_second(second: i64) -> SystemTime {
        if second < 0 {
            UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS - second.unsigned_abs())
        } else {
            UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS + second as u64)
        }
    }

    /// Hardware whose `n`th sample reads `n × 100` bytes total and `n` available, except the
    /// `empty`th, which reads nothing; sample `gate.sample` signals its start and waits for
    /// release.
    struct CountingHardware {
        samples: AtomicU64,
        gate: Option<Gate>,
        empty: Option<u64>,
    }

    struct Gate {
        sample: u64,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl HardwareSnapshotSource for CountingHardware {
        fn collect_snapshot(&self) -> HardwareSnapshot {
            HardwareSnapshot::default()
        }

        fn sample(&self) -> HardwareSample {
            let sample = self.samples.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(gate) = self.gate.as_ref().filter(|gate| gate.sample == sample) {
                gate.entered.send(()).expect("test waits for the sample");
                gate.release
                    .lock()
                    .expect("gate")
                    .recv()
                    .expect("test releases the sample");
            }
            if self.empty == Some(sample) {
                return HardwareSample::default();
            }
            HardwareSample {
                host: HostSample {
                    memory: Some(HostMemorySample {
                        total_bytes: sample * 100,
                        available_bytes: sample,
                        cached_bytes: None,
                    }),
                    ..HostSample::default()
                },
                accelerators: vec![],
            }
        }
    }

    struct ChannelRecorder(tokio::sync::mpsc::UnboundedSender<Vec<MetricRecord>>);

    impl MetricRecorder for ChannelRecorder {
        fn record(&self, records: Vec<MetricRecord>) {
            let _ = self.0.send(records);
        }
    }
}
