//! Takes metric bucket streams from SiteNodes into the site manager's metric store, merging each
//! batch exactly once however often a node sends it.
//!
//! A batch is acknowledged only once the store has taken it, waiting for room rather than dropping
//! it, so an acknowledged bucket is never lost while the manager runs. One stream per node is
//! live: a newer stream of the same node ends the older one, and a stream silent past the idle
//! timeout ends on the backend clock. The manager's own node streams in process through the same
//! path. A node only writes the node-measured series of itself: the store rejects and counts any
//! other series, and the batch carrying it is still acknowledged.

use std::{
    collections::HashMap,
    collections::VecDeque,
    future::Future,
    sync::{Arc, Mutex},
};

use dotcore_domain::{clock::Clock, timing::MetricsTiming};
use futures_util::{
    StreamExt,
    future::{self, BoxFuture},
    stream::{self, BoxStream},
};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::{
    bucket_stream::{
        BucketBatch, BucketStreamConnection, BucketStreamError, BucketStreamOpen,
        MetricBucketTransport,
    },
    retention::aligned_bucket_start,
    store::MetricIngest,
};

/// Sessions remembered per node for deduplication, so a forwarder of an older process that still
/// sends during a restart never merges a batch twice.
const REMEMBERED_SESSIONS: usize = 4;

/// Batches an in-process stream queues toward the ingest.
const IN_PROCESS_QUEUE: usize = 16;

/// The site manager's receiver of node metric bucket streams.
pub struct MetricBucketIngest {
    store: Arc<dyn MetricIngest>,
    clock: Arc<dyn Clock>,
    timing: MetricsTiming,
    nodes: Mutex<HashMap<String, Arc<NodeStreams>>>,
}

impl MetricBucketIngest {
    /// Creates an ingest writing into `store` that ends streams silent for the stream silence of
    /// `timing`, measured on `clock`.
    pub fn new(store: Arc<dyn MetricIngest>, timing: MetricsTiming, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            timing,
            nodes: Mutex::default(),
        }
    }

    /// Waits for a stream's first message for at most the idle timeout; `None` when it did not
    /// arrive in time.
    pub async fn first_message<F: Future>(&self, message: F) -> Option<F::Output> {
        let deadline = self.clock.now() + self.timing.stream_silence;
        tokio::select! {
            message = message => Some(message),
            () = self.clock.sleep_until(deadline) => None,
        }
    }

    /// Serves one stream: applies each batch of `batches` and yields the session's cumulative
    /// applied sequence after each. It replaces any earlier stream of the same node and ends after
    /// its first error, or when `batches` ends.
    pub fn serve(
        self: &Arc<Self>,
        open: BucketStreamOpen,
        batches: BoxStream<'static, Result<BucketBatch, BucketStreamEnd>>,
    ) -> BoxStream<'static, Result<u64, BucketStreamEnd>> {
        let node = Arc::clone(
            self.nodes
                .lock()
                .expect("metric bucket ingest lock")
                .entry(open.node_id.clone())
                .or_default(),
        );
        node.generation.send_modify(|generation| *generation += 1);
        // Subscribing marks this stream's own generation as seen, so only a newer one wakes it.
        let replaced = node.generation.subscribe();
        let generation = *replaced.borrow();
        debug!(
            event_kind = "metric_bucket_stream_opened",
            node_id = open.node_id,
            session_id = open.session_id,
            "metric bucket stream opened"
        );
        let stream = NodeStream {
            ingest: Arc::clone(self),
            node,
            open,
            generation,
            replaced,
            batches,
            last_sequence: 0,
        };
        stream::unfold(Some(stream), |stream| async move {
            let mut stream = stream?;
            match stream.next_applied().await {
                Some(Ok(applied)) => Some((Ok(applied), Some(stream))),
                Some(Err(end)) => {
                    debug!(
                        event_kind = "metric_bucket_stream_ended",
                        node_id = stream.open.node_id,
                        reason = %end,
                        "metric bucket stream ended"
                    );
                    Some((Err(end), None))
                }
                None => None,
            }
        })
        .boxed()
    }
}

/// Why the manager ended a metric bucket stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BucketStreamEnd {
    #[error("a newer metric bucket stream of the same node replaced this one")]
    Replaced,

    #[error("the node sent nothing for the idle timeout")]
    Idle,

    #[error("the metric store stopped")]
    StoreStopped,

    #[error("metric bucket batch sequences must be nonzero and increase within a stream")]
    OutOfOrder,

    #[error("metric bucket is not aligned to its nonzero width")]
    MisalignedBucket,

    #[error("metric bucket stream message is malformed")]
    Malformed,

    #[error("the node's side of the metric bucket stream failed")]
    Interrupted,
}

/// Streams of a node's forwarder into the store over the manager's own ingest, without a
/// network hop.
pub struct InProcessBucketTransport {
    ingest: Arc<MetricBucketIngest>,
}

impl InProcessBucketTransport {
    /// Creates a transport into `ingest`.
    pub fn new(ingest: Arc<MetricBucketIngest>) -> Self {
        Self { ingest }
    }
}

impl MetricBucketTransport for InProcessBucketTransport {
    fn open(
        &self,
        open: BucketStreamOpen,
    ) -> BoxFuture<'_, Result<BucketStreamConnection, BucketStreamError>> {
        let (sender, receiver) = mpsc::channel(IN_PROCESS_QUEUE);
        let batches = stream::unfold(receiver, |mut receiver| async move {
            let batch = receiver.recv().await?;
            Some((Ok(batch), receiver))
        })
        .boxed();
        let applied = self
            .ingest
            .serve(open, batches)
            .map(|applied| applied.map_err(BucketStreamError::new))
            .boxed();
        Box::pin(future::ready(Ok(BucketStreamConnection {
            batches: sender,
            applied,
        })))
    }
}

/// Streams of one node: the recent sessions they share and the generation of the live stream.
#[derive(Default)]
struct NodeStreams {
    /// The node's most recently seen sessions, newest first, at most [`REMEMBERED_SESSIONS`].
    sessions: tokio::sync::Mutex<VecDeque<AppliedSession>>,
    generation: watch::Sender<u64>,
}

/// One session of a node and the highest sequence of it the store has taken.
struct AppliedSession {
    id: String,
    applied: u64,
}

/// One served stream.
struct NodeStream {
    ingest: Arc<MetricBucketIngest>,
    node: Arc<NodeStreams>,
    open: BucketStreamOpen,
    generation: u64,
    replaced: watch::Receiver<u64>,
    batches: BoxStream<'static, Result<BucketBatch, BucketStreamEnd>>,
    last_sequence: u64,
}

impl NodeStream {
    /// Waits for the next batch and applies it; `None` once the node closed its side.
    async fn next_applied(&mut self) -> Option<Result<u64, BucketStreamEnd>> {
        let deadline = self.ingest.clock.now() + self.ingest.timing.stream_silence;
        tokio::select! {
            biased;
            // The sender lives in `node`, which this stream holds, so it never closes.
            _ = self.replaced.changed() => Some(Err(BucketStreamEnd::Replaced)),
            batch = self.batches.next() => match batch? {
                Ok(batch) => Some(self.apply(batch).await),
                Err(end) => Some(Err(end)),
            },
            () = self.ingest.clock.sleep_until(deadline) => Some(Err(BucketStreamEnd::Idle)),
        }
    }

    async fn apply(&mut self, batch: BucketBatch) -> Result<u64, BucketStreamEnd> {
        if batch.sequence <= self.last_sequence {
            return Err(BucketStreamEnd::OutOfOrder);
        }
        self.last_sequence = batch.sequence;
        let aligned = batch.buckets.iter().all(|bucket| {
            !bucket.width.is_zero()
                && aligned_bucket_start(bucket.start, bucket.width) == bucket.start
        });
        if !aligned {
            return Err(BucketStreamEnd::MisalignedBucket);
        }

        // Held while the store takes the batch, so a replacing stream re-sending it waits and
        // then sees it applied.
        let mut sessions = self.node.sessions.lock().await;
        if *self.node.generation.borrow() != self.generation {
            return Err(BucketStreamEnd::Replaced);
        }
        let position = sessions
            .iter()
            .position(|session| session.id == self.open.session_id);
        let mut session = match position {
            Some(position) => sessions.remove(position).expect("remembered session"),
            None => AppliedSession {
                id: self.open.session_id.clone(),
                applied: 0,
            },
        };
        if batch.sequence <= session.applied {
            debug!(
                event_kind = "metric_bucket_batch_duplicate",
                node_id = self.open.node_id,
                sequence = batch.sequence,
                "metric bucket batch already applied"
            );
            let applied = session.applied;
            remember(&mut sessions, session);
            return Ok(applied);
        }
        let records = batch
            .buckets
            .into_iter()
            .flat_map(|bucket| bucket.into_records())
            .collect();
        let stored = self
            .ingest
            .store
            .ingest(&self.open.node_id, records, batch.backfill)
            .await;
        if stored.is_ok() {
            session.applied = batch.sequence;
        }
        let applied = session.applied;
        remember(&mut sessions, session);
        stored.map_err(|_| BucketStreamEnd::StoreStopped)?;
        Ok(applied)
    }
}

/// Puts `session` first among the node's remembered sessions, forgetting the oldest beyond
/// [`REMEMBERED_SESSIONS`].
fn remember(sessions: &mut VecDeque<AppliedSession>, session: AppliedSession) {
    sessions.push_front(session);
    sessions.truncate(REMEMBERED_SESSIONS);
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        time::{Duration, SystemTime},
    };

    use dotcore_domain::{clock::ManualClock, timing::Timing};
    use futures_util::FutureExt;

    use super::*;
    use crate::{
        aggregate::{Aggregate, GaugeAggregate},
        bucket_stream::{BucketSeries, MetricBucket},
        descriptor::{MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricUnit},
        labels::LabelSet,
        memory::{InMemoryMetricStore, InMemoryStoreConfig},
        query::{MetricQuery, PointValue, QueryRange, Reduce, SeriesAggregate},
        store::MetricStore,
    };

    #[tokio::test]
    async fn a_resent_batch_is_acknowledged_without_merging_again() {
        let (store, clock, ingest) = start();
        let bucket = second(clock.wall_now()) - Duration::from_secs(5);
        let later = bucket + Duration::from_secs(1);
        let first = batch(1, vec![memory_bucket(bucket, "n1", &[4.0, 6.0])]);
        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        sender.send(Ok(first.clone())).expect("send batch");
        assert_eq!(next(&mut stream).await, Some(Ok(1)));

        // The acknowledgement was lost: the node reconnects and sends the batch again.
        let (resender, mut resent) = open_stream(&ingest, "n1", "session-a");
        assert_eq!(
            next(&mut stream).await,
            Some(Err(BucketStreamEnd::Replaced))
        );
        assert_eq!(next(&mut stream).await, None);
        resender.send(Ok(first)).expect("resend batch");
        resender
            .send(Ok(batch(2, vec![memory_bucket(later, "n1", &[8.0])])))
            .expect("send next batch");

        assert_eq!(next(&mut resent).await, Some(Ok(1)));
        assert_eq!(next(&mut resent).await, Some(Ok(2)));
        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Sum).await,
            Some(PointValue::Gauge(10.0))
        );
        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Avg).await,
            Some(PointValue::Gauge(5.0))
        );
        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Last).await,
            Some(PointValue::Gauge(6.0))
        );
        assert_eq!(
            gauge(&store, "n1", later, Reduce::Sum).await,
            Some(PointValue::Gauge(8.0))
        );
    }

    #[tokio::test]
    async fn a_new_session_applies_its_sequences_from_the_start() {
        let (store, clock, ingest) = start();
        let bucket = second(clock.wall_now()) - Duration::from_secs(5);
        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        sender
            .send(Ok(batch(3, vec![memory_bucket(bucket, "n1", &[1.0])])))
            .expect("send batch");
        assert_eq!(next(&mut stream).await, Some(Ok(3)));

        // A restarted forwarder numbers its batches from 1 again.
        let (restarted, mut stream) = open_stream(&ingest, "n1", "session-b");
        restarted
            .send(Ok(batch(1, vec![memory_bucket(bucket, "n1", &[2.0])])))
            .expect("send batch");

        assert_eq!(next(&mut stream).await, Some(Ok(1)));
        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Sum).await,
            Some(PointValue::Gauge(3.0))
        );
    }

    #[tokio::test]
    async fn a_burst_larger_than_the_store_queue_loses_no_acknowledged_bucket() {
        // Twice the store's queue of record batches, all handed in before the ingest runs.
        const BATCHES: u64 = 2048;
        let (store, clock, ingest) = start();
        let newest = second(clock.wall_now());
        let at = |sequence: u64| newest - Duration::from_secs(sequence % 600);
        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        for sequence in 1..=BATCHES {
            // One series per batch, so every batch's value stays separately readable.
            let mut bucket = memory_bucket(at(sequence), "n1", &[sequence as f64]);
            bucket.series[0].labels = LabelSet::new()
                .with("node", "n1")
                .with("device", format!("d{sequence}"));
            sender
                .send(Ok(batch(sequence, vec![bucket])))
                .expect("send batch");
        }

        // Unconstrained, the ingest yields to the store only when the store's queue is full.
        tokio::task::unconstrained(async {
            for sequence in 1..=BATCHES {
                assert_eq!(next(&mut stream).await, Some(Ok(sequence)));
            }
        })
        .await;
        for sequence in 1..=BATCHES {
            assert_eq!(
                read(
                    &store,
                    ("device", &format!("d{sequence}")),
                    at(sequence),
                    Reduce::Sum
                )
                .await,
                Some(PointValue::Gauge(sequence as f64)),
                "batch {sequence}"
            );
        }
        assert_eq!(store.dropped_records(), 0);
        assert_eq!(store.rejected_records(), 0);
    }

    #[tokio::test]
    async fn buckets_from_before_the_store_started_are_acknowledged_but_not_stored() {
        let (store, clock, ingest) = start();
        let started = store.incarnation().started_at;
        let before = second(started) - Duration::from_secs(1);
        let after = second(clock.wall_now()) - Duration::from_secs(1);
        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        sender
            .send(Ok(batch(
                1,
                vec![
                    memory_bucket(before, "n1", &[1.0]),
                    memory_bucket(after, "n1", &[2.0]),
                ],
            )))
            .expect("send batch");

        assert_eq!(next(&mut stream).await, Some(Ok(1)));
        assert_eq!(gauge(&store, "n1", before, Reduce::Sum).await, None);
        assert_eq!(
            gauge(&store, "n1", after, Reduce::Sum).await,
            Some(PointValue::Gauge(2.0))
        );
        assert_eq!(store.rejected_records(), 1);
    }

    #[tokio::test]
    async fn only_node_measured_series_of_the_stream_node_are_stored() {
        let (store, clock, ingest) = start();
        let bucket = second(clock.wall_now()) - Duration::from_secs(5);
        let mut mixed = memory_bucket(bucket, "n1", &[4.0]);
        mixed
            .series
            .extend(memory_bucket(bucket, "n2", &[7.0]).series);
        // A manager-measured metric without any node label.
        mixed.series.push(BucketSeries {
            metric: "router.request.in_flight".to_owned(),
            labels: LabelSet::new().with("route", "route-a"),
            aggregate: Aggregate::Gauge(GaugeAggregate::from_sample(3.0, bucket)),
        });
        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        sender.send(Ok(batch(1, vec![mixed]))).expect("send batch");

        // The batch is acknowledged; its rejected series are counted, not re-sent.
        assert_eq!(next(&mut stream).await, Some(Ok(1)));
        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Sum).await,
            Some(PointValue::Gauge(4.0))
        );
        assert_eq!(gauge(&store, "n2", bucket, Reduce::Sum).await, None);
        let in_flight = store
            .query(MetricQuery {
                metric: "router.request.in_flight".to_owned(),
                matchers: BTreeMap::new(),
                group_by: vec![],
                reduce: Some(Reduce::Sum),
                aggregate: Some(SeriesAggregate::Sum),
                percentiles: vec![],
                range: QueryRange::Last(Duration::from_secs(60)),
                step_hint: Duration::from_secs(1),
            })
            .await
            .expect("query");
        assert_eq!(in_flight.series, vec![]);
        assert_eq!(store.rejected_records(), 2);
    }

    #[tokio::test]
    async fn alternating_sessions_of_one_node_never_merge_a_batch_twice() {
        let (store, clock, ingest) = start();
        let bucket = second(clock.wall_now()) - Duration::from_secs(5);
        // An old forwarder process still sending during a restart alternates with the new one.
        for (session, value) in [("session-a", 1.0), ("session-b", 2.0)] {
            let (sender, mut stream) = open_stream(&ingest, "n1", session);
            sender
                .send(Ok(batch(1, vec![memory_bucket(bucket, "n1", &[value])])))
                .expect("send batch");
            assert_eq!(next(&mut stream).await, Some(Ok(1)));
        }

        for (session, value) in [("session-a", 1.0), ("session-b", 2.0)] {
            let (sender, mut stream) = open_stream(&ingest, "n1", session);
            sender
                .send(Ok(batch(1, vec![memory_bucket(bucket, "n1", &[value])])))
                .expect("resend batch");
            assert_eq!(next(&mut stream).await, Some(Ok(1)));
        }

        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Sum).await,
            Some(PointValue::Gauge(3.0))
        );
        assert_eq!(
            gauge(&store, "n1", bucket, Reduce::Avg).await,
            Some(PointValue::Gauge(1.5))
        );
    }

    #[tokio::test]
    async fn a_first_message_must_arrive_within_the_idle_timeout() {
        let (_store, clock, ingest) = start();

        let arrived = ingest.first_message(std::future::ready("open")).await;
        let mut silent = Box::pin(ingest.first_message(std::future::pending::<()>()));
        // The first poll starts the wait.
        assert_eq!((&mut silent).now_or_never(), None);
        clock.advance(Timing::test().metrics.stream_silence);

        assert_eq!(arrived, Some("open"));
        assert_eq!(silent.await, None);
    }

    #[tokio::test]
    async fn a_silent_stream_ends_after_the_idle_timeout() {
        let (_store, clock, ingest) = start();
        let mut stream = ingest.serve(open("n1", "session-a"), stream::pending().boxed());
        // The first poll starts the idle wait.
        assert_eq!(stream.next().now_or_never(), None);

        clock.advance(Timing::test().metrics.stream_silence);

        assert_eq!(next(&mut stream).await, Some(Err(BucketStreamEnd::Idle)));
        assert_eq!(next(&mut stream).await, None);
    }

    #[tokio::test]
    async fn invalid_batches_end_the_stream() {
        let (_store, clock, ingest) = start();
        let bucket = second(clock.wall_now()) - Duration::from_secs(5);
        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        sender
            .send(Ok(batch(2, vec![memory_bucket(bucket, "n1", &[1.0])])))
            .expect("send batch");
        sender
            .send(Ok(batch(2, vec![memory_bucket(bucket, "n1", &[1.0])])))
            .expect("send batch");
        assert_eq!(next(&mut stream).await, Some(Ok(2)));
        assert_eq!(
            next(&mut stream).await,
            Some(Err(BucketStreamEnd::OutOfOrder))
        );

        let (sender, mut stream) = open_stream(&ingest, "n1", "session-a");
        sender
            .send(Ok(batch(
                3,
                vec![memory_bucket(
                    bucket + Duration::from_millis(500),
                    "n1",
                    &[1.0],
                )],
            )))
            .expect("send batch");
        assert_eq!(
            next(&mut stream).await,
            Some(Err(BucketStreamEnd::MisalignedBucket))
        );
    }

    type BatchSender = mpsc::UnboundedSender<Result<BucketBatch, BucketStreamEnd>>;

    type AppliedStream = BoxStream<'static, Result<u64, BucketStreamEnd>>;

    fn start() -> (
        Arc<InMemoryMetricStore>,
        ManualClock,
        Arc<MetricBucketIngest>,
    ) {
        let clock = ManualClock::new();
        let store = Arc::new(
            InMemoryMetricStore::start(
                InMemoryStoreConfig::default(),
                vec![
                    MetricDescriptor {
                        name: "node.memory.used".to_owned(),
                        kind: MetricKind::Gauge,
                        unit: MetricUnit::Bytes,
                        dimensions: vec![
                            MetricDimension::required("node"),
                            MetricDimension::optional("device"),
                        ],
                        provenance: MetricProvenance::Node,
                        sample_interval: Some(Duration::from_secs(1)),
                        reporting_delay: None,
                    },
                    MetricDescriptor {
                        name: "router.request.in_flight".to_owned(),
                        kind: MetricKind::Gauge,
                        unit: MetricUnit::Requests,
                        dimensions: vec![MetricDimension::required("route")],
                        provenance: MetricProvenance::Manager,
                        sample_interval: Some(Duration::from_secs(1)),
                        reporting_delay: None,
                    },
                ],
                Timing::test().metrics,
                Timing::test().watch,
                Arc::new(clock.clone()),
            )
            .expect("start store"),
        );
        let ingest = Arc::new(MetricBucketIngest::new(
            store.ingestor(),
            Timing::test().metrics,
            Arc::new(clock.clone()),
        ));
        // Leaves ten minutes of history after the store start for buckets to land in.
        clock.advance(Duration::from_secs(11 * 60));
        (store, clock, ingest)
    }

    fn open(node_id: &str, session_id: &str) -> BucketStreamOpen {
        BucketStreamOpen {
            node_id: node_id.to_owned(),
            session_id: session_id.to_owned(),
        }
    }

    /// Serves a stream whose batches the test sends.
    fn open_stream(
        ingest: &Arc<MetricBucketIngest>,
        node_id: &str,
        session_id: &str,
    ) -> (BatchSender, AppliedStream) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let batches = stream::unfold(receiver, |mut receiver| async move {
            let batch = receiver.recv().await?;
            Some((batch, receiver))
        })
        .boxed();
        (sender, ingest.serve(open(node_id, session_id), batches))
    }

    async fn next(stream: &mut AppliedStream) -> Option<Result<u64, BucketStreamEnd>> {
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("metric bucket stream deadline")
    }

    fn batch(sequence: u64, buckets: Vec<MetricBucket>) -> BucketBatch {
        BucketBatch {
            sequence,
            buckets,
            backfill: false,
        }
    }

    /// A one-second bucket of `node.memory.used` for `node` holding `readings`, observed in order
    /// inside the bucket.
    fn memory_bucket(start: SystemTime, node: &str, readings: &[f64]) -> MetricBucket {
        let mut summary: Option<GaugeAggregate> = None;
        for (index, reading) in readings.iter().enumerate() {
            let sample = GaugeAggregate::from_sample(
                *reading,
                start + Duration::from_millis(100 * (index as u64 + 1)),
            );
            match &mut summary {
                Some(summary) => summary.merge(&sample),
                None => summary = Some(sample),
            }
        }
        MetricBucket {
            start,
            width: Duration::from_secs(1),
            series: vec![BucketSeries {
                metric: "node.memory.used".to_owned(),
                labels: LabelSet::new().with("node", node),
                aggregate: Aggregate::Gauge(summary.expect("one reading")),
            }],
        }
    }

    /// The `reduce` reading of `node`'s one-second bucket starting at `bucket`.
    async fn gauge(
        store: &InMemoryMetricStore,
        node: &str,
        bucket: SystemTime,
        reduce: Reduce,
    ) -> Option<PointValue> {
        read(store, ("node", node), bucket, reduce).await
    }

    /// The `reduce` reading of the one-second bucket starting at `bucket` of the series whose
    /// label `matcher.0` equals `matcher.1`.
    async fn read(
        store: &InMemoryMetricStore,
        (label, value): (&str, &str),
        bucket: SystemTime,
        reduce: Reduce,
    ) -> Option<PointValue> {
        let result = store
            .query(MetricQuery {
                metric: "node.memory.used".to_owned(),
                matchers: BTreeMap::from([(label.to_owned(), value.to_owned())]),
                group_by: vec![],
                reduce: Some(reduce),
                aggregate: Some(SeriesAggregate::Sum),
                percentiles: vec![],
                range: QueryRange::Between {
                    start: bucket,
                    end: bucket + Duration::from_secs(1),
                },
                step_hint: Duration::from_secs(1),
            })
            .await
            .expect("query");
        let series = result.series.into_iter().next()?;
        series
            .points
            .into_iter()
            .find(|point| point.bucket_start == bucket)
            .map(|point| point.value)
    }

    fn second(time: SystemTime) -> SystemTime {
        aligned_bucket_start(time, Duration::from_secs(1))
    }
}
