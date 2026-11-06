//! Forwards a SiteNode's metrics to the site manager: writers on the node record samples, which
//! the forwarder summarizes into one-second buckets aligned to the Unix epoch and streams to the
//! manager until it acknowledges them.
//!
//! A bucket stays open until a short grace past its end on the backend clock, so writers sampling
//! at different moments, or a little late, all land in it; a sample for a bucket already sent is
//! dropped and counted. Unacknowledged batches stay in a bounded buffer, so a brief disconnect
//! leaves no gap: after a reconnect they are sent again, oldest first and a limited number at a
//! time, marked as backfill. The buffer is transient, not retention; when full, its oldest
//! batches are dropped and counted. Each bucket also records `node.observing{node, source} = 1`
//! for every writer that recorded in it, so readers can tell a writer that measured nothing from
//! one that was not running, and the records the forwarder dropped since its previous bucket.

use std::{
    collections::{BTreeMap, VecDeque, btree_map::Entry},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use dotcore_domain::clock::Clock;
use futures_util::StreamExt;
use tokio::{sync::Notify, task::JoinHandle, time::Instant};
use tracing::{debug, warn};

use crate::{
    aggregate::{Aggregate, CounterAggregate, GaugeAggregate},
    bucket_stream::{
        BucketBatch, BucketSeries, BucketStreamConnection, BucketStreamOpen, MetricBucket,
        MetricBucketTransport,
    },
    descriptor::{MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricUnit},
    labels::{self, LabelSet},
    record::{MetricRecord, RecordValue},
    retention::aligned_bucket_start,
    store::MetricRecorder,
};

/// Whether a writer on a node recorded in a bucket: 1 when it did, absent when it did not.
pub const NODE_OBSERVING: &str = "node.observing";

/// Records a node's forwarder dropped, by reason: `buffer_full` for the series of a batch dropped
/// from a full buffer, `stale` for a sample whose bucket was already sent, `unsupported` for a
/// sample the forwarder cannot summarize.
pub const FORWARDER_DROPPED: &str = "observability.forwarder.dropped";

/// Width of the buckets a forwarder summarizes samples into.
// Not a behavior duration: the one-second metric bucket contract.
const BUCKET_WIDTH: Duration = Duration::from_secs(1);

/// Batches sent on a stream and not yet acknowledged before the forwarder waits for an ack, so
/// re-sending after a long disconnect is paced by the manager.
const MAX_IN_FLIGHT: usize = 16;

/// Estimated memory of one buffered batch beyond its series.
const BATCH_OVERHEAD_BYTES: usize = 64;

/// Estimated memory of one buffered series beyond its metric name and labels.
const SERIES_OVERHEAD_BYTES: usize = 96;

/// Declarations of the metrics a forwarder records itself.
pub fn forwarder_metric_descriptors() -> Vec<MetricDescriptor> {
    vec![
        MetricDescriptor {
            name: NODE_OBSERVING.to_owned(),
            kind: MetricKind::Gauge,
            unit: MetricUnit::Ratio,
            dimensions: vec![
                MetricDimension::required(labels::NODE),
                MetricDimension::required(labels::SOURCE),
            ],
            provenance: MetricProvenance::Node,
            sample_interval: Some(BUCKET_WIDTH),
            reporting_delay: None,
        },
        MetricDescriptor {
            name: FORWARDER_DROPPED.to_owned(),
            kind: MetricKind::Counter,
            unit: MetricUnit::Records,
            dimensions: vec![
                MetricDimension::required(labels::NODE),
                MetricDimension::required(labels::REASON),
            ],
            provenance: MetricProvenance::Node,
            sample_interval: None,
            reporting_delay: None,
        },
    ]
}

/// Bounds and timing of a forwarder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwarderConfig {
    /// How long past its end a bucket stays open for samples still on their way.
    pub flush_grace: Duration,

    /// Oldest buffered bucket kept, relative to the newest one. A sample older than the last sent
    /// bucket by more than this means the wall clock was set back, and the forwarder starts over.
    pub max_buffer_age: Duration,

    /// Estimated memory the buffered batches may take.
    pub max_buffer_bytes: usize,

    /// How long a sent batch may wait for an acknowledgement before the forwarder reconnects.
    pub ack_timeout: Duration,

    /// Pause before opening a stream again after one failed or ended.
    pub reconnect_delay: Duration,
}

impl Default for ForwarderConfig {
    /// A half-second flush grace, which keeps buckets inside the manager's two-second closing
    /// grace for node metrics; a buffer of 15 minutes (the finest retained resolution) and 8 MiB; a
    /// 10 second acknowledgement timeout matching node liveness; and a one second reconnect pause.
    fn default() -> Self {
        Self {
            flush_grace: Duration::from_millis(500),
            max_buffer_age: Duration::from_secs(15 * 60),
            max_buffer_bytes: 8 * 1024 * 1024,
            ack_timeout: Duration::from_secs(10),
            reconnect_delay: Duration::from_secs(1),
        }
    }
}

/// A node's metric forwarder; clones share it.
#[derive(Clone)]
pub struct MetricForwarder {
    shared: Arc<Forwarder>,
}

impl MetricForwarder {
    /// Starts the forwarder of SiteNode `node_id`, streaming through `transport` and flushing and
    /// timing on `clock`. Must run inside a Tokio runtime; the forwarder's task stops when the
    /// returned task handle is dropped.
    pub fn start(
        node_id: &str,
        transport: Arc<dyn MetricBucketTransport>,
        clock: Arc<dyn Clock>,
        config: ForwarderConfig,
    ) -> Result<(Self, MetricForwarderTask), ForwarderStartError> {
        let mut session = [0_u8; 16];
        getrandom::fill(&mut session).map_err(ForwarderStartError::Session)?;
        let shared = Arc::new(Forwarder {
            node_id: node_id.to_owned(),
            session_id: session.iter().map(|byte| format!("{byte:02x}")).collect(),
            config,
            state: Mutex::new(ForwarderState {
                open: BTreeMap::new(),
                flushed_through: None,
                next_sequence: 1,
                pending: VecDeque::new(),
                pending_bytes: 0,
                disconnected: false,
                unreported_drops: BTreeMap::new(),
            }),
            opened: Notify::new(),
            flushed: Notify::new(),
            dropped_batches: AtomicU64::new(0),
            stale_records: AtomicU64::new(0),
            unsupported_records: AtomicU64::new(0),
        });
        let task = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move {
                tokio::join!(
                    flush_loop(Arc::clone(&shared), Arc::clone(&clock)),
                    stream_loop(shared, transport, clock),
                );
            }
        });
        Ok((Self { shared }, MetricForwarderTask(task)))
    }

    /// A write-only handle for the writer named `source`, such as `hardware`.
    pub fn recorder(&self, source: &str) -> Arc<dyn MetricRecorder> {
        Arc::new(SourceRecorder {
            shared: Arc::clone(&self.shared),
            source: source.to_owned(),
        })
    }

    /// Buffered batches dropped because the buffer was full.
    pub fn dropped_batches(&self) -> u64 {
        self.shared.dropped_batches.load(Ordering::Relaxed)
    }

    /// Records dropped because their bucket was already sent.
    pub fn stale_records(&self) -> u64 {
        self.shared.stale_records.load(Ordering::Relaxed)
    }

    /// Records dropped because the forwarder summarizes only gauge readings and counter
    /// increments.
    pub fn unsupported_records(&self) -> u64 {
        self.shared.unsupported_records.load(Ordering::Relaxed)
    }

    /// Flushed batches not yet acknowledged and their estimated memory.
    #[cfg(test)]
    fn buffered(&self) -> (usize, usize) {
        let state = self.shared.state.lock().expect("metric forwarder lock");
        (state.pending.len(), state.pending_bytes)
    }
}

/// The running task of a forwarder, stopped when dropped.
pub struct MetricForwarderTask(JoinHandle<()>);

impl Drop for MetricForwarderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A forwarder that cannot start.
#[derive(Debug, thiserror::Error)]
pub enum ForwarderStartError {
    #[error("generate the metric forwarder session")]
    Session(#[source] getrandom::Error),
}

/// State shared by a forwarder's recorders and its task.
struct Forwarder {
    node_id: String,
    session_id: String,
    config: ForwarderConfig,
    state: Mutex<ForwarderState>,

    /// Wakes the flush loop when a bucket opens.
    opened: Notify,

    /// Wakes the stream loop when a batch is flushed.
    flushed: Notify,

    dropped_batches: AtomicU64,
    stale_records: AtomicU64,
    unsupported_records: AtomicU64,
}

impl Forwarder {
    fn record(&self, source: &str, records: Vec<MetricRecord>) {
        let mut state = self.state.lock().expect("metric forwarder lock");
        let mut opened = false;
        for record in records {
            let reading = match record.value {
                RecordValue::Gauge(value) => {
                    Aggregate::Gauge(GaugeAggregate::from_sample(value, record.observed_at))
                }
                RecordValue::Counter(delta) => {
                    Aggregate::Counter(CounterAggregate::from_delta(delta))
                }
                RecordValue::Distribution(_) | RecordValue::Aggregate { .. } => {
                    self.unsupported_records.fetch_add(1, Ordering::Relaxed);
                    *state.unreported_drops.entry("unsupported").or_default() += 1;
                    debug!(
                        event_kind = "metric_forwarder_record_unsupported",
                        metric = record.metric,
                        "metric forwarder dropped a record that is neither a gauge reading nor a \
                         counter increment"
                    );
                    continue;
                }
            };
            let start = aligned_bucket_start(record.observed_at, BUCKET_WIDTH);
            if let Some(flushed) = state.flushed_through
                && start <= flushed
            {
                if start + self.config.max_buffer_age >= flushed {
                    self.stale_records.fetch_add(1, Ordering::Relaxed);
                    *state.unreported_drops.entry("stale").or_default() += 1;
                    continue;
                }
                // Far older than anything a late writer sends: the wall clock was set back.
                // Sending what is open and starting over loses less than dropping every sample
                // until the clock catches up.
                warn!(
                    event_kind = "metric_forwarder_clock_reset",
                    behind_seconds = flushed.duration_since(start).unwrap_or_default().as_secs(),
                    "metric forwarder saw the wall clock set back; starting new buckets"
                );
                while let Some((start, open)) = state.open.pop_first() {
                    self.flush(&mut state, start, open);
                }
                state.flushed_through = None;
            }

            let open = state.open.entry(start).or_insert_with(|| {
                opened = true;
                OpenBucket::default()
            });
            match open.series.entry((record.metric, record.labels)) {
                Entry::Vacant(entry) => {
                    entry.insert(reading);
                }
                Entry::Occupied(mut entry) => {
                    if entry.get_mut().merge(&reading).is_err() {
                        self.unsupported_records.fetch_add(1, Ordering::Relaxed);
                        *state.unreported_drops.entry("unsupported").or_default() += 1;
                        continue;
                    }
                }
            }
            let observed = open
                .observing
                .entry(source.to_owned())
                .or_insert(record.observed_at);
            *observed = (*observed).max(record.observed_at);
        }
        if opened {
            self.opened.notify_one();
        }
    }

    /// Flushes every open bucket whose grace ended by `now`, oldest first, and returns when the
    /// next one is due.
    fn flush_due(&self, now: SystemTime) -> Option<SystemTime> {
        let mut state = self.state.lock().expect("metric forwarder lock");
        loop {
            let (&start, _) = state.open.first_key_value()?;
            let due = start + BUCKET_WIDTH + self.config.flush_grace;
            if due > now {
                return Some(due);
            }
            let open = state.open.remove(&start).expect("first open bucket");
            self.flush(&mut state, start, open);
        }
    }

    /// Moves one open bucket into the buffer as the next batch, dropping the oldest batches past
    /// the buffer bounds.
    fn flush(&self, state: &mut ForwarderState, start: SystemTime, open: OpenBucket) {
        let observing = open
            .observing
            .into_iter()
            .map(|(source, observed_at)| BucketSeries {
                metric: NODE_OBSERVING.to_owned(),
                labels: LabelSet::new()
                    .with(labels::NODE, self.node_id.as_str())
                    .with(labels::SOURCE, source),
                aggregate: Aggregate::Gauge(GaugeAggregate::from_sample(1.0, observed_at)),
            });
        let dropped =
            std::mem::take(&mut state.unreported_drops)
                .into_iter()
                .map(|(reason, count)| BucketSeries {
                    metric: FORWARDER_DROPPED.to_owned(),
                    labels: LabelSet::new()
                        .with(labels::NODE, self.node_id.as_str())
                        .with(labels::REASON, reason),
                    aggregate: Aggregate::Counter(CounterAggregate::from_delta(count as f64)),
                });
        let series = open
            .series
            .into_iter()
            .map(|((metric, labels), aggregate)| BucketSeries {
                metric,
                labels,
                aggregate,
            })
            .chain(observing)
            .chain(dropped)
            .collect();
        let bucket = MetricBucket {
            start,
            width: BUCKET_WIDTH,
            series,
        };
        let bytes = estimated_bytes(&bucket);
        state.pending.push_back(PendingBatch {
            sequence: state.next_sequence,
            bucket,
            bytes,
            backfill: state.disconnected,
        });
        state.next_sequence += 1;
        state.pending_bytes += bytes;
        state.flushed_through = Some(
            state
                .flushed_through
                .map_or(start, |flushed| flushed.max(start)),
        );

        let newest = state
            .pending
            .iter()
            .map(|batch| batch.bucket.start)
            .max()
            .expect("a buffered batch");
        while let Some(oldest) = state.pending.front() {
            let too_old = oldest.bucket.start + self.config.max_buffer_age <= newest;
            if !too_old && state.pending_bytes <= self.config.max_buffer_bytes {
                break;
            }
            let dropped = state.pending.pop_front().expect("oldest buffered batch");
            state.pending_bytes -= dropped.bytes;
            self.dropped_batches.fetch_add(1, Ordering::Relaxed);
            let series = dropped
                .bucket
                .series
                .iter()
                .filter(|series| series.metric != FORWARDER_DROPPED)
                .count() as u64;
            *state.unreported_drops.entry("buffer_full").or_default() += series;
            debug!(
                event_kind = "metric_forwarder_batch_dropped",
                sequence = dropped.sequence,
                "metric forwarder buffer is full; oldest batch dropped"
            );
        }
        self.flushed.notify_one();
    }

    /// Marks a stream as open, so batches flushed from now on are sent for the first time.
    fn connected(&self) {
        self.state
            .lock()
            .expect("metric forwarder lock")
            .disconnected = false;
    }

    /// Marks every buffered batch, and every batch flushed until the next stream opens, as sent
    /// again after a stream failed or ended.
    fn disconnected(&self) {
        let mut state = self.state.lock().expect("metric forwarder lock");
        state.disconnected = true;
        for batch in &mut state.pending {
            batch.backfill = true;
        }
    }

    /// Forgets every buffered batch up to and including `applied`.
    fn acknowledge(&self, applied: u64) {
        let mut state = self.state.lock().expect("metric forwarder lock");
        while state
            .pending
            .front()
            .is_some_and(|batch| batch.sequence <= applied)
        {
            let acknowledged = state.pending.pop_front().expect("acknowledged batch");
            state.pending_bytes -= acknowledged.bytes;
        }
    }

    /// The next buffered batch after `sent_through` when fewer than [`MAX_IN_FLIGHT`] are sent
    /// and unacknowledged, and whether any batch is in flight.
    fn next_to_send(&self, sent_through: u64) -> (Option<BucketBatch>, bool) {
        let state = self.state.lock().expect("metric forwarder lock");
        let in_flight = state
            .pending
            .iter()
            .take_while(|batch| batch.sequence <= sent_through)
            .count();
        let next = (in_flight < MAX_IN_FLIGHT)
            .then(|| {
                state
                    .pending
                    .iter()
                    .find(|batch| batch.sequence > sent_through)
            })
            .flatten()
            .map(|batch| BucketBatch {
                sequence: batch.sequence,
                buckets: vec![batch.bucket.clone()],
                backfill: batch.backfill,
            });
        (next, in_flight > 0)
    }

    /// Completes once a batch is buffered.
    async fn wait_pending(&self) {
        loop {
            let flushed = self.flushed.notified();
            if !self
                .state
                .lock()
                .expect("metric forwarder lock")
                .pending
                .is_empty()
            {
                return;
            }
            flushed.await;
        }
    }

    /// Sends buffered batches on `connection` and trims them as acknowledged, until the stream
    /// fails, ends, or leaves a batch unacknowledged past the timeout.
    async fn stream(&self, mut connection: BucketStreamConnection, clock: &dyn Clock) -> StreamEnd {
        let mut sent_through = 0;
        let mut waiting_since: Option<Instant> = None;
        loop {
            let in_flight = loop {
                let (next, in_flight) = self.next_to_send(sent_through);
                let Some(batch) = next else {
                    break in_flight;
                };
                if !in_flight {
                    waiting_since = Some(clock.now());
                }
                sent_through = batch.sequence;
                if connection.batches.send(batch).await.is_err() {
                    return StreamEnd::Closed;
                }
            };
            if !in_flight {
                waiting_since = None;
            }

            let flushed = self.flushed.notified();
            let ack_deadline = waiting_since.map(|since| since + self.config.ack_timeout);
            tokio::select! {
                applied = connection.applied.next() => match applied {
                    Some(Ok(applied)) => {
                        self.acknowledge(applied);
                        waiting_since = Some(clock.now());
                    }
                    Some(Err(_)) => return StreamEnd::Failed,
                    None => return StreamEnd::Closed,
                },
                () = flushed => {}
                () = sleep_until_some(clock, ack_deadline) => return StreamEnd::AckTimeout,
            }
        }
    }
}

/// Buckets of a forwarder: the open ones and the flushed ones awaiting acknowledgement.
struct ForwarderState {
    /// Buckets still taking samples, by start.
    open: BTreeMap<SystemTime, OpenBucket>,

    /// Start of the latest flushed bucket; records for it or an earlier one are stale.
    flushed_through: Option<SystemTime>,

    next_sequence: u64,

    /// Flushed batches not yet acknowledged, oldest first.
    pending: VecDeque<PendingBatch>,

    pending_bytes: usize,

    /// Whether a stream failed or ended and no new one opened since.
    disconnected: bool,

    /// Records dropped since the last flushed bucket, by reason.
    unreported_drops: BTreeMap<&'static str, u64>,
}

/// One open bucket: summaries by series and, per writer, its latest observation.
#[derive(Default)]
struct OpenBucket {
    series: BTreeMap<(String, LabelSet), Aggregate>,
    observing: BTreeMap<String, SystemTime>,
}

struct PendingBatch {
    sequence: u64,
    bucket: MetricBucket,
    bytes: usize,

    /// Whether the batch is sent again after a stream failed or ended.
    backfill: bool,
}

struct SourceRecorder {
    shared: Arc<Forwarder>,
    source: String,
}

impl MetricRecorder for SourceRecorder {
    fn record(&self, records: Vec<MetricRecord>) {
        self.shared.record(&self.source, records);
    }
}

/// Why one stream stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEnd {
    Closed,
    Failed,
    AckTimeout,
}

/// Flushes each open bucket once its grace has passed on the clock, waking at the next due
/// bucket or when one opens.
async fn flush_loop(shared: Arc<Forwarder>, clock: Arc<dyn Clock>) {
    loop {
        let opened = shared.opened.notified();
        let now = clock.wall_now();
        match shared.flush_due(now) {
            Some(due) => {
                let wait = due.duration_since(now).unwrap_or_default();
                tokio::select! {
                    () = clock.sleep_until(clock.now() + wait) => {}
                    () = opened => {}
                }
            }
            None => opened.await,
        }
    }
}

/// Opens streams while batches are buffered and reconnects after each one ends.
async fn stream_loop(
    shared: Arc<Forwarder>,
    transport: Arc<dyn MetricBucketTransport>,
    clock: Arc<dyn Clock>,
) {
    loop {
        shared.wait_pending().await;
        let open = BucketStreamOpen {
            node_id: shared.node_id.clone(),
            session_id: shared.session_id.clone(),
        };
        match transport.open(open).await {
            Ok(connection) => {
                shared.connected();
                let end = shared.stream(connection, clock.as_ref()).await;
                debug!(
                    event_kind = "metric_forwarder_stream_ended",
                    reason = ?end,
                    "metric forwarder stream ended"
                );
            }
            Err(error) => {
                debug!(
                    event_kind = "metric_forwarder_stream_failed",
                    error = %error,
                    "metric forwarder could not open its stream"
                );
            }
        }
        shared.disconnected();
        clock
            .sleep_until(clock.now() + shared.config.reconnect_delay)
            .await;
    }
}

/// Completes at `deadline`, or never without one.
async fn sleep_until_some(clock: &dyn Clock, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => clock.sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Approximate memory a buffered bucket takes.
fn estimated_bytes(bucket: &MetricBucket) -> usize {
    BATCH_OVERHEAD_BYTES
        + bucket
            .series
            .iter()
            .map(|series| {
                SERIES_OVERHEAD_BYTES
                    + series.metric.len()
                    + series
                        .labels
                        .iter()
                        .map(|(name, value)| name.len() + value.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::UNIX_EPOCH};

    use dotcore_domain::clock::VirtualClock;
    use futures_util::{future::BoxFuture, stream};
    use tokio::sync::{Semaphore, mpsc};

    use super::*;
    use crate::{
        bucket_stream::BucketStreamError,
        ingest::{InProcessBucketTransport, MetricBucketIngest},
        memory::{InMemoryMetricStore, InMemoryStoreConfig},
        query::{MetricQuery, PointValue, QueryRange, Reduce, SeriesAggregate},
        store::MetricStore,
    };

    #[tokio::test]
    async fn a_bucket_flushes_once_its_grace_past_the_end_has_passed() {
        let mut harness = Harness::start(ForwarderConfig::default());
        let hardware = harness.forwarder.recorder("hardware");
        hardware.record(vec![
            memory("n1", at(10_200), 4.0),
            memory("n2", at(10_400), 1.0),
        ]);
        hardware.record(vec![memory("n1", at(10_700), 6.0)]);

        // Second 10 ends at 11.0; its grace runs until 11.5.
        harness.clock.advance(Duration::from_millis(1_499));
        assert_eq!(harness.forwarder.buffered().0, 0);
        harness.clock.advance(Duration::from_millis(1));

        let mut stream = harness.next_stream().await;
        assert_eq!(stream.open.node_id, NODE);
        assert_eq!(stream.open.session_id.len(), 32);
        assert_eq!(
            stream.next_batch().await,
            BucketBatch {
                sequence: 1,
                buckets: vec![MetricBucket {
                    start: at(10_000),
                    width: Duration::from_secs(1),
                    series: vec![
                        memory_series("n1", gauge(2, 10.0, 4.0, 6.0, 6.0, at(10_700))),
                        memory_series("n2", gauge(1, 1.0, 1.0, 1.0, 1.0, at(10_400))),
                        observing_series("hardware", at(10_700)),
                    ],
                }],
                backfill: false,
            }
        );
    }

    #[tokio::test]
    async fn a_late_writer_lands_in_a_bucket_another_writer_has_moved_past() {
        let mut harness = Harness::start(ForwarderConfig::default());
        let hardware = harness.forwarder.recorder("hardware");
        let engine = harness.forwarder.recorder("engine");

        // The hardware writer samples seconds 10 and 11 before the engine writer reports its
        // reading from 10.6, still within second 10's grace.
        hardware.record(vec![memory("n1", at(10_000), 1.0)]);
        harness.clock.advance(Duration::from_secs(1));
        hardware.record(vec![memory("n1", at(11_000), 2.0)]);
        harness.clock.advance(Duration::from_millis(300));
        engine.record(vec![kv_cache(at(10_600), 0.5)]);
        engine.record(vec![kv_cache(at(11_200), 0.75)]);
        harness.clock.advance(Duration::from_millis(1_200));

        let mut stream = harness.next_stream().await;
        assert_eq!(
            stream.next_batch().await,
            BucketBatch {
                sequence: 1,
                buckets: vec![MetricBucket {
                    start: at(10_000),
                    width: Duration::from_secs(1),
                    series: vec![
                        kv_cache_series(gauge(1, 0.5, 0.5, 0.5, 0.5, at(10_600))),
                        memory_series("n1", gauge(1, 1.0, 1.0, 1.0, 1.0, at(10_000))),
                        observing_series("engine", at(10_600)),
                        observing_series("hardware", at(10_000)),
                    ],
                }],
                backfill: false,
            }
        );
        assert_eq!(
            stream.next_batch().await,
            BucketBatch {
                sequence: 2,
                buckets: vec![MetricBucket {
                    start: at(11_000),
                    width: Duration::from_secs(1),
                    series: vec![
                        kv_cache_series(gauge(1, 0.75, 0.75, 0.75, 0.75, at(11_200))),
                        memory_series("n1", gauge(1, 2.0, 2.0, 2.0, 2.0, at(11_000))),
                        observing_series("engine", at(11_200)),
                        observing_series("hardware", at(11_000)),
                    ],
                }],
                backfill: false,
            }
        );
        assert_eq!(harness.forwarder.stale_records(), 0);
    }

    #[tokio::test]
    async fn a_sample_for_a_sent_bucket_is_stale() {
        let mut harness = Harness::start(ForwarderConfig::default());
        let hardware = harness.forwarder.recorder("hardware");
        hardware.record(vec![memory("n1", at(10_200), 1.0)]);
        harness.clock.advance(Duration::from_millis(1_500));
        let mut stream = harness.next_stream().await;
        assert_eq!(stream.next_batch().await.sequence, 1);

        hardware.record(vec![memory("n1", at(10_900), 2.0)]);
        hardware.record(vec![memory("n1", at(11_600), 3.0)]);
        harness.clock.advance(Duration::from_secs(1));

        assert_eq!(harness.forwarder.stale_records(), 1);
        let next = stream.next_batch().await;
        assert_eq!((next.sequence, next.buckets[0].start), (2, at(11_000)));
        assert_eq!(
            next.buckets[0].series,
            vec![
                memory_series("n1", gauge(1, 3.0, 3.0, 3.0, 3.0, at(11_600))),
                observing_series("hardware", at(11_600)),
                dropped_series("stale", 1.0),
            ]
        );
    }

    #[tokio::test]
    async fn a_wall_clock_set_far_back_starts_new_buckets_instead_of_dropping_samples() {
        let mut harness = Harness::start(ForwarderConfig {
            max_buffer_age: Duration::from_secs(60),
            ..ForwarderConfig::default()
        });
        let hardware = harness.forwarder.recorder("hardware");
        hardware.record(vec![memory("n1", at(10_200), 1.0)]);
        harness.clock.advance(Duration::from_millis(1_500));
        let mut stream = harness.next_stream().await;
        assert_eq!(stream.next_batch().await.buckets[0].start, at(10_000));

        // Within the bound a sample for a sent bucket is stale; beyond it the clock was reset.
        harness.clock.step_wall_back(Duration::from_secs(5));
        hardware.record(vec![memory("n1", at(6_500), 2.0)]);
        harness.clock.step_wall_back(Duration::from_secs(120));
        hardware.record(vec![memory("n1", at(-113_500), 3.0)]);
        harness.clock.advance(Duration::from_secs(1));

        assert_eq!(harness.forwarder.stale_records(), 1);
        let reset = stream.next_batch().await;
        assert_eq!((reset.sequence, reset.buckets[0].start), (2, at(-114_000)));
        assert_eq!(
            reset.buckets[0].series[0],
            memory_series("n1", gauge(1, 3.0, 3.0, 3.0, 3.0, at(-113_500)))
        );
    }

    #[tokio::test]
    async fn acknowledged_batches_leave_the_buffer() {
        let mut harness = Harness::start(ForwarderConfig::default());
        record_seconds(&harness.forwarder, 10..=12);
        harness.clock.advance(Duration::from_millis(3_500));
        let mut stream = harness.next_stream().await;
        for sequence in 1..=3 {
            assert_eq!(stream.next_batch().await.sequence, sequence);
        }
        assert_eq!(harness.forwarder.buffered().0, 3);

        stream.acknowledge(2);
        eventually(|| harness.forwarder.buffered().0 == 1).await;
        stream.acknowledge(3);
        eventually(|| harness.forwarder.buffered() == (0, 0)).await;
    }

    #[tokio::test]
    async fn the_buffer_drops_its_oldest_batches_past_its_age_bound() {
        let mut harness = Harness::start_gated(
            ForwarderConfig {
                max_buffer_age: Duration::from_secs(3),
                ..ForwarderConfig::default()
            },
            0,
        );

        // Seconds 10 to 16 flush; only 14, 15 and 16 are within 3 seconds of the newest.
        record_seconds(&harness.forwarder, 10..=16);
        harness.clock.advance(Duration::from_millis(7_500));

        eventually(|| harness.forwarder.dropped_batches() == 4).await;
        assert_eq!(harness.forwarder.buffered().0, 3);
        harness.gate.add_permits(1);
        let mut stream = harness.next_stream().await;
        // Each kept batch reports the memory and observing series of the batch dropped before it.
        for (sequence, second) in [(5, 14), (6, 15), (7, 16)] {
            let batch = stream.next_batch().await;
            assert_eq!(batch.sequence, sequence);
            assert_eq!(batch.buckets[0].start, at(second * 1000));
            assert_eq!(
                batch.buckets[0].series.last(),
                Some(&dropped_series("buffer_full", 2.0))
            );
        }
    }

    #[tokio::test]
    async fn the_buffer_drops_its_oldest_batches_past_its_memory_bound() {
        let measured = Harness::start_gated(ForwarderConfig::default(), 0);
        record_seconds(&measured.forwarder, 10..=10);
        measured.clock.advance(Duration::from_millis(1_500));
        eventually(|| measured.forwarder.buffered().0 == 1).await;
        let batch_bytes = measured.forwarder.buffered().1;
        let mut harness = Harness::start_gated(
            ForwarderConfig {
                max_buffer_bytes: batch_bytes * 2 + batch_bytes / 2,
                ..ForwarderConfig::default()
            },
            0,
        );

        record_seconds(&harness.forwarder, 10..=14);
        harness.clock.advance(Duration::from_millis(5_500));

        // Batches after a drop also report it, so they are larger and the exact number kept
        // depends on the estimate; the newest batches are kept within the bound.
        eventually(|| {
            harness.forwarder.dropped_batches() as usize + harness.forwarder.buffered().0 == 5
        })
        .await;
        let (kept, bytes) = harness.forwarder.buffered();
        assert!(harness.forwarder.dropped_batches() >= 3);
        assert!(kept >= 1);
        assert!(bytes <= batch_bytes * 2 + batch_bytes / 2);
        harness.gate.add_permits(1);
        let mut stream = harness.next_stream().await;
        for sequence in (5 - kept as u64 + 1)..=5 {
            assert_eq!(stream.next_batch().await.sequence, sequence);
        }
    }

    #[tokio::test]
    async fn unacknowledged_batches_are_sent_again_after_a_reconnect() {
        let mut harness = Harness::start_gated(
            ForwarderConfig {
                reconnect_delay: Duration::ZERO,
                ..ForwarderConfig::default()
            },
            1,
        );
        record_seconds(&harness.forwarder, 10..=11);
        harness.clock.advance(Duration::from_millis(2_500));
        let mut lost = harness.next_stream().await;
        for sequence in 1..=2 {
            let batch = lost.next_batch().await;
            assert_eq!((batch.sequence, batch.backfill), (sequence, false));
        }

        // The stream ends unacknowledged, and one more bucket flushes while the forwarder has
        // none.
        lost.close();
        record_seconds(&harness.forwarder, 12..=12);
        harness.clock.advance(Duration::from_secs(1));
        eventually(|| harness.forwarder.buffered().0 == 3).await;
        harness.gate.add_permits(1);
        let mut stream = harness.next_stream().await;

        for (sequence, second) in [(1, 10), (2, 11), (3, 12)] {
            let batch = stream.next_batch().await;
            assert_eq!(
                (batch.sequence, batch.buckets[0].start, batch.backfill),
                (sequence, at(second * 1000), true)
            );
        }

        // Flushed while the new stream is open, so sent for the first time.
        record_seconds(&harness.forwarder, 13..=13);
        harness.clock.advance(Duration::from_secs(1));
        let fresh = stream.next_batch().await;
        assert_eq!((fresh.sequence, fresh.backfill), (4, false));
    }

    #[tokio::test]
    async fn a_batch_left_unacknowledged_past_the_timeout_reconnects() {
        let mut harness = Harness::start(ForwarderConfig {
            ack_timeout: Duration::from_secs(10),
            reconnect_delay: Duration::ZERO,
            ..ForwarderConfig::default()
        });
        record_seconds(&harness.forwarder, 10..=10);
        harness.clock.advance(Duration::from_millis(1_500));
        let mut silent = harness.next_stream().await;
        assert_eq!(silent.next_batch().await.sequence, 1);

        harness.clock.advance(Duration::from_secs(10));

        let mut stream = harness.next_stream().await;
        assert!(silent.closed().await);
        assert_eq!(stream.next_batch().await.sequence, 1);
    }

    #[tokio::test]
    async fn gauge_readings_and_counter_increments_are_forwarded_and_others_counted() {
        let mut harness = Harness::start(ForwarderConfig::default());
        let requests = |at, delta| {
            MetricRecord::counter(
                "node.requests",
                LabelSet::new().with("node", NODE),
                at,
                delta,
            )
        };

        harness.forwarder.recorder("hardware").record(vec![
            requests(at(10_100), 2.0),
            requests(at(10_400), 3.0),
            MetricRecord::distribution(
                "node.latency",
                LabelSet::new().with("node", NODE),
                at(10_200),
                0.5,
            ),
        ]);
        record_seconds(&harness.forwarder, 10..=10);
        harness.clock.advance(Duration::from_millis(1_500));

        let batch = harness.next_stream().await.next_batch().await;
        assert_eq!(harness.forwarder.unsupported_records(), 1);
        assert_eq!(
            batch.buckets[0].series,
            vec![
                memory_series("n1", gauge(1, 10.0, 10.0, 10.0, 10.0, at(10_000))),
                BucketSeries {
                    metric: "node.requests".to_owned(),
                    labels: LabelSet::new().with("node", NODE),
                    aggregate: Aggregate::Counter(CounterAggregate::from_delta(5.0)),
                },
                observing_series("hardware", at(10_400)),
                dropped_series("unsupported", 1.0),
            ]
        );
    }

    #[tokio::test]
    async fn the_manager_node_forwards_in_process_into_the_store() {
        let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS));
        let mut descriptors = forwarder_metric_descriptors();
        descriptors.push(memory_descriptor());
        let store = InMemoryMetricStore::start(
            InMemoryStoreConfig::default(),
            descriptors,
            Arc::new(clock.clone()),
        )
        .expect("start store");
        let ingest = Arc::new(MetricBucketIngest::new(
            store.ingestor(),
            Arc::new(clock.clone()),
        ));
        clock.advance(Duration::from_secs(60));
        let (forwarder, _task) = MetricForwarder::start(
            NODE,
            Arc::new(InProcessBucketTransport::new(ingest)),
            Arc::new(clock.clone()),
            ForwarderConfig::default(),
        )
        .expect("start forwarder");
        // Five seconds old, so its grace has already passed.
        let bucket = aligned_bucket_start(clock.wall_now(), BUCKET_WIDTH) - Duration::from_secs(5);

        forwarder.recorder("hardware").record(vec![
            memory(NODE, bucket + Duration::from_millis(100), 3.0),
            memory(NODE, bucket + Duration::from_millis(600), 5.0),
        ]);

        eventually(|| {
            forwarder.buffered().0 == 0
                && forwarder
                    .shared
                    .state
                    .lock()
                    .expect("forwarder state")
                    .open
                    .is_empty()
        })
        .await;
        let read = |metric: &str, reduce: Reduce| {
            let query = MetricQuery {
                metric: metric.to_owned(),
                matchers: BTreeMap::new(),
                group_by: vec![],
                reduce: Some(reduce),
                aggregate: Some(SeriesAggregate::Sum),
                percentiles: vec![],
                range: QueryRange::Between {
                    start: bucket,
                    end: bucket + BUCKET_WIDTH,
                },
                step_hint: BUCKET_WIDTH,
            };
            let store = &store;
            async move {
                let result = store.query(query).await.expect("query");
                result.series[0].points[0].clone()
            }
        };
        let memory = read("node.memory.used", Reduce::Sum).await;
        assert_eq!(memory.bucket_start, bucket);
        assert_eq!(memory.value, PointValue::Gauge(8.0));
        assert_eq!(
            read("node.memory.used", Reduce::Last).await.value,
            PointValue::Gauge(5.0)
        );
        assert_eq!(
            read(NODE_OBSERVING, Reduce::Sum).await.value,
            PointValue::Gauge(1.0)
        );
    }

    const NODE: &str = "node-a";

    /// Whole seconds after the Unix epoch the harness clock's wall time starts at, second 10 of
    /// the test timeline.
    const EPOCH_SECONDS: u64 = 1_700_000_010;

    /// A forwarder on a virtual clock that starts at second 10, streaming through a transport the
    /// test drives.
    struct Harness {
        forwarder: MetricForwarder,
        _task: MetricForwarderTask,
        clock: VirtualClock,
        gate: Arc<Semaphore>,
        streams: mpsc::UnboundedReceiver<FakeStream>,
    }

    impl Harness {
        fn start(config: ForwarderConfig) -> Self {
            Self::start_gated(config, Semaphore::MAX_PERMITS)
        }

        /// Starts a forwarder whose transport opens only `opens` streams until the test adds
        /// permits to the gate.
        fn start_gated(config: ForwarderConfig, opens: usize) -> Self {
            let clock = VirtualClock::starting_at(at(10_000));
            let gate = Arc::new(Semaphore::new(opens));
            let (opened, streams) = mpsc::unbounded_channel();
            let (forwarder, task) = MetricForwarder::start(
                NODE,
                Arc::new(FakeTransport {
                    gate: Arc::clone(&gate),
                    opened,
                }),
                Arc::new(clock.clone()),
                config,
            )
            .expect("start forwarder");
            Self {
                forwarder,
                _task: task,
                clock,
                gate,
                streams,
            }
        }

        async fn next_stream(&mut self) -> FakeStream {
            tokio::time::timeout(Duration::from_secs(10), self.streams.recv())
                .await
                .expect("stream open deadline")
                .expect("transport open")
        }
    }

    struct FakeTransport {
        gate: Arc<Semaphore>,
        opened: mpsc::UnboundedSender<FakeStream>,
    }

    impl MetricBucketTransport for FakeTransport {
        fn open(
            &self,
            open: BucketStreamOpen,
        ) -> BoxFuture<'_, Result<BucketStreamConnection, BucketStreamError>> {
            Box::pin(async move {
                self.gate.acquire().await.expect("open gate").forget();
                let (batches, received) = mpsc::channel(64);
                let (applied, acknowledgements) = mpsc::unbounded_channel();
                let _ = self.opened.send(FakeStream {
                    open,
                    batches: received,
                    applied: Some(applied),
                });
                Ok(BucketStreamConnection {
                    batches,
                    applied: stream::unfold(acknowledgements, |mut acknowledgements| async move {
                        let applied = acknowledgements.recv().await?;
                        Some((applied, acknowledgements))
                    })
                    .boxed(),
                })
            })
        }
    }

    /// The manager's ends of one stream, driven by the test.
    struct FakeStream {
        open: BucketStreamOpen,
        batches: mpsc::Receiver<BucketBatch>,
        applied: Option<mpsc::UnboundedSender<Result<u64, BucketStreamError>>>,
    }

    impl FakeStream {
        async fn next_batch(&mut self) -> BucketBatch {
            tokio::time::timeout(Duration::from_secs(10), self.batches.recv())
                .await
                .expect("batch deadline")
                .expect("stream open")
        }

        fn acknowledge(&self, applied: u64) {
            self.applied
                .as_ref()
                .expect("stream open")
                .send(Ok(applied))
                .expect("forwarder reads acknowledgements");
        }

        /// Ends the manager's side of the stream.
        fn close(&mut self) {
            self.applied = None;
        }

        /// Whether the forwarder dropped its side, once it has.
        async fn closed(&mut self) -> bool {
            tokio::time::timeout(Duration::from_secs(10), self.batches.recv())
                .await
                .expect("stream close deadline")
                .is_none()
        }
    }

    /// Records one `n1` memory reading at the start of every second in `seconds`.
    fn record_seconds(forwarder: &MetricForwarder, seconds: std::ops::RangeInclusive<i64>) {
        let hardware = forwarder.recorder("hardware");
        for second in seconds {
            hardware.record(vec![memory("n1", at(second * 1000), second as f64)]);
        }
    }

    fn memory(node: &str, observed_at: SystemTime, value: f64) -> MetricRecord {
        MetricRecord::gauge(
            "node.memory.used",
            LabelSet::new().with("node", node),
            observed_at,
            value,
        )
    }

    fn kv_cache(observed_at: SystemTime, value: f64) -> MetricRecord {
        MetricRecord::gauge(
            "engine.kv_cache.usage",
            kv_cache_labels(),
            observed_at,
            value,
        )
    }

    fn kv_cache_labels() -> LabelSet {
        LabelSet::new()
            .with("node", NODE)
            .with("deployment", "deployment-a")
    }

    fn memory_descriptor() -> MetricDescriptor {
        MetricDescriptor {
            name: "node.memory.used".to_owned(),
            kind: MetricKind::Gauge,
            unit: MetricUnit::Bytes,
            dimensions: vec![MetricDimension::required("node")],
            provenance: MetricProvenance::Node,
            sample_interval: Some(BUCKET_WIDTH),
            reporting_delay: None,
        }
    }

    fn memory_series(node: &str, summary: GaugeAggregate) -> BucketSeries {
        BucketSeries {
            metric: "node.memory.used".to_owned(),
            labels: LabelSet::new().with("node", node),
            aggregate: Aggregate::Gauge(summary),
        }
    }

    fn kv_cache_series(summary: GaugeAggregate) -> BucketSeries {
        BucketSeries {
            metric: "engine.kv_cache.usage".to_owned(),
            labels: kv_cache_labels(),
            aggregate: Aggregate::Gauge(summary),
        }
    }

    fn observing_series(source: &str, observed_at: SystemTime) -> BucketSeries {
        BucketSeries {
            metric: NODE_OBSERVING.to_owned(),
            labels: LabelSet::new().with("node", NODE).with("source", source),
            aggregate: Aggregate::Gauge(GaugeAggregate::from_sample(1.0, observed_at)),
        }
    }

    fn dropped_series(reason: &str, count: f64) -> BucketSeries {
        BucketSeries {
            metric: FORWARDER_DROPPED.to_owned(),
            labels: LabelSet::new().with("node", NODE).with("reason", reason),
            aggregate: Aggregate::Counter(CounterAggregate::from_delta(count)),
        }
    }

    fn gauge(
        count: u64,
        sum: f64,
        min: f64,
        max: f64,
        last: f64,
        last_observed_at: SystemTime,
    ) -> GaugeAggregate {
        GaugeAggregate::from_parts(count, sum, min, max, last, last_observed_at)
            .expect("consistent gauge summary")
    }

    /// `millis` after second 0 of the test timeline, which may be negative.
    fn at(millis: i64) -> SystemTime {
        let zero = UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS - 10);
        if millis < 0 {
            zero - Duration::from_millis(millis.unsigned_abs())
        } else {
            zero + Duration::from_millis(millis as u64)
        }
    }

    /// Polls `condition` until it holds, failing after a generous deadline.
    async fn eventually(condition: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "condition not reached before the deadline"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
