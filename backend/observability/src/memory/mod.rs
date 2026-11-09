//! Keeps metric history in process memory for one store lifetime, at every retention tier at once.
//!
//! Writers hand records through a bounded queue and never wait; a background task merges each
//! record into the epoch-aligned bucket of every tier still covering its time, keeping only
//! buckets that received data. A query first drains every record handed in before it, so callers
//! read their own writes. Watches follow the same history live, told by the ingest task which
//! buckets each batch changed. History starts when the store starts and is lost when the process
//! stops, so a bucket a writer pre-aggregated from before the store started is rejected rather
//! than merged partially.
//!
//! Retention time alone bounds memory: a periodic sweep trims every series to retention, removes
//! series left without history, and records the store's own health as metrics.

mod evaluate;
mod health;
mod watch;

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use dotcore_domain::{background_loop::log_unexpected_loop_end, clock::Clock, timing::Timing};
use futures_util::future::BoxFuture;
use tokio::{
    sync::{Semaphore, broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};
use tracing::debug;

use self::health::{HealthCounts, UpReadings};
use crate::{
    aggregate::{Aggregate, CounterAggregate, DistributionAggregate, GaugeAggregate},
    descriptor::{
        DescriptorError, MetricDescriptor, MetricKind, MetricProvenance, validate_catalog,
    },
    labels::{self, LabelSet},
    query::{MetricQuery, QueryError, QueryResult, StoreIncarnation},
    record::{MetricRecord, RecordValue},
    retention::{RetentionConfig, align_down, unix_millis, width_millis},
    store::{MetricIngest, MetricRecorder, MetricStore, StoreStopped},
    watch::{MetricWatch, WatchError},
};

/// Record batches queued toward the store before recorders start dropping.
const RECORDER_CAPACITY: usize = 1024;

/// Merged record batches a watch may trail behind before it ends as lagged.
const CHANGES_CAPACITY: usize = 1024;

/// How far past the store's wall time a record may be observed, absorbing small clock skew
/// between machines.
/// Not a behavior duration: tolerance for clock differences between machines.
const FUTURE_TOLERANCE: Duration = Duration::from_secs(5);

/// Estimated memory of one series beyond its buckets, metric name and labels.
const SERIES_OVERHEAD_BYTES: usize = 128;

/// In-memory metric history for one store lifetime.
pub struct InMemoryMetricStore {
    shared: Arc<Shared>,
    ingest: mpsc::Sender<Ingest>,
    changes: broadcast::Sender<ChangeBatch>,
    watches: Arc<Semaphore>,
    max_open_queries: usize,
    task: JoinHandle<()>,
}

impl InMemoryMetricStore {
    /// Starts a store accepting `descriptors`, plus the metrics it records about itself, and
    /// keeping history per `config`. Must run inside a Tokio runtime, which hosts the store's
    /// ingest task until the store is dropped.
    pub fn start(
        config: InMemoryStoreConfig,
        descriptors: Vec<MetricDescriptor>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StoreStartError> {
        let InMemoryStoreConfig {
            retention,
            max_open_queries,
            sweep_interval,
            watch_heartbeat_interval,
        } = config;
        let finest = retention.tiers()[0].width;
        let descriptors: Vec<_> = descriptors
            .into_iter()
            .chain(health::health_metric_descriptors(finest, sweep_interval))
            .collect();
        validate_catalog(&descriptors)?;
        let declared: BTreeMap<_, _> = descriptors
            .into_iter()
            .map(|descriptor| (descriptor.name.clone(), descriptor))
            .collect();

        let mut id = [0_u8; 16];
        getrandom::fill(&mut id).map_err(StoreStartError::Incarnation)?;
        let incarnation = StoreIncarnation {
            id: id.iter().map(|byte| format!("{byte:02x}")).collect(),
            started_at: clock.wall_now(),
        };

        let shared = Arc::new(Shared {
            clock,
            sweep_interval,
            watch_heartbeat_interval,
            retention,
            descriptors: declared,
            incarnation,
            series: Mutex::default(),
            dropped_records: AtomicU64::new(0),
            rejected_records: AtomicU64::new(0),
        });
        // The bucket the store starts in counts as observed before any query is served.
        let (up, first_up) =
            UpReadings::start(finest, shared.clock.now(), shared.incarnation.started_at);
        merge_health_records(
            &shared,
            &mut shared.series.lock().expect("metric store lock"),
            unix_millis(shared.incarnation.started_at),
            vec![first_up],
            None,
        );

        let (ingest, receiver) = mpsc::channel(RECORDER_CAPACITY);
        // The ingest task holds the only other sender, so watches see the channel close once the
        // store is dropped and its task aborted.
        let (changes, _) = broadcast::channel(CHANGES_CAPACITY);
        let task = tokio::spawn(log_unexpected_loop_end(
            "metric_store_ingest",
            ingest_loop(Arc::clone(&shared), receiver, changes.clone(), up),
        ));
        Ok(Self {
            shared,
            ingest,
            changes,
            watches: Arc::new(Semaphore::new(max_open_queries)),
            max_open_queries,
            task,
        })
    }

    /// A write-only handle for code that measures metrics.
    pub fn recorder(&self) -> Arc<dyn MetricRecorder> {
        Arc::new(QueuedRecorder {
            ingest: self.ingest.clone(),
            shared: Arc::clone(&self.shared),
        })
    }

    /// A write handle that waits for room in the store, for writers that confirm delivery onward.
    pub fn ingestor(&self) -> Arc<dyn MetricIngest> {
        Arc::new(QueuedRecorder {
            ingest: self.ingest.clone(),
            shared: Arc::clone(&self.shared),
        })
    }

    /// This store lifetime's identity and start.
    pub fn incarnation(&self) -> &StoreIncarnation {
        &self.shared.incarnation
    }

    /// Records dropped because the ingest queue was full.
    pub fn dropped_records(&self) -> u64 {
        self.shared.dropped_records.load(Ordering::Relaxed)
    }

    /// Records rejected as undeclared, mislabeled, of the wrong kind, outside retention, or
    /// pre-aggregated over a bucket that started before the store.
    pub fn rejected_records(&self) -> u64 {
        self.shared.rejected_records.load(Ordering::Relaxed)
    }

    /// Waits until every record handed to a recorder before this call is merged.
    async fn drain(&self) -> Result<(), QueryError> {
        let (done, drained) = oneshot::channel();
        self.ingest
            .send(Ingest::Drain(done))
            .await
            .map_err(|_| QueryError::StoreStopped)?;
        drained.await.map_err(|_| QueryError::StoreStopped)
    }
}

impl MetricStore for InMemoryMetricStore {
    fn descriptors(&self) -> Vec<MetricDescriptor> {
        self.shared.descriptors.values().cloned().collect()
    }

    fn retention(&self) -> RetentionConfig {
        self.shared.retention.clone()
    }

    fn query(&self, query: MetricQuery) -> BoxFuture<'_, Result<QueryResult, QueryError>> {
        Box::pin(async move {
            self.drain().await?;
            evaluate::evaluate(&self.shared, &query)
        })
    }

    fn watch(&self, query: MetricQuery) -> BoxFuture<'_, Result<MetricWatch, WatchError>> {
        Box::pin(async move {
            let permit = Arc::clone(&self.watches).try_acquire_owned().map_err(|_| {
                WatchError::TooManyWatches {
                    limit: self.max_open_queries,
                }
            })?;
            // Subscribing before the drain means every batch merged after the backfill read is
            // also announced, so no change falls between backfill and live updates.
            let changes = self.changes.subscribe();
            self.drain().await?;
            watch::open(Arc::clone(&self.shared), query, changes, permit)
        })
    }
}

impl Drop for InMemoryMetricStore {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Settings of an in-memory metric store.
///
/// The open-query cap bounds CPU and lock time rather than memory: the point limit bounds only
/// each query's backfill, but at every bucket close each open query evaluates again under the
/// store's single series lock, which ingest also takes, scanning every series in the store and
/// every change batch. A round therefore costs open queries times series; at the default of 1024
/// open queries it takes a few milliseconds on a site-sized store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMemoryStoreConfig {
    /// Resolutions and how long history is kept at each.
    pub retention: RetentionConfig,

    /// Queries watched at once across the store; every open watch is one query and counts once.
    pub max_open_queries: usize,

    /// How often the store trims expired history and records its own health.
    pub sweep_interval: Duration,

    /// Longest a watch stays silent: an idle watch then sends a point-less upsert, so a vanished
    /// watcher's broken connection is noticed and its watch closed.
    pub watch_heartbeat_interval: Duration,
}

impl InMemoryStoreConfig {
    /// These settings with the sweep and watch heartbeat intervals of `timing`.
    pub fn with_timing(self, timing: &Timing) -> Self {
        Self {
            sweep_interval: timing.metric_store_sweep_interval,
            watch_heartbeat_interval: timing.watch_heartbeat_interval,
            ..self
        }
    }
}

impl Default for InMemoryStoreConfig {
    fn default() -> Self {
        let production = Timing::production();
        Self {
            retention: RetentionConfig::default(),
            max_open_queries: 1024,
            sweep_interval: production.metric_store_sweep_interval,
            watch_heartbeat_interval: production.watch_heartbeat_interval,
        }
    }
}

/// A store that cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StoreStartError {
    #[error("invalid metric declaration")]
    Descriptor(#[from] DescriptorError),

    #[error("generate the metric store incarnation")]
    Incarnation(#[source] getrandom::Error),
}

/// State shared by the store, its recorders and its ingest task.
struct Shared {
    clock: Arc<dyn Clock>,
    sweep_interval: Duration,
    watch_heartbeat_interval: Duration,
    retention: RetentionConfig,
    descriptors: BTreeMap<String, MetricDescriptor>,
    incarnation: StoreIncarnation,
    series: Mutex<HashMap<SeriesKey, Series>>,
    dropped_records: AtomicU64,
    rejected_records: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SeriesKey {
    metric: String,
    labels: LabelSet,
}

/// Retained buckets of one series, one sparse map per retention tier keyed by bucket start in
/// Unix milliseconds.
struct Series {
    tiers: Vec<BTreeMap<u64, Aggregate>>,
}

enum Ingest {
    /// Records of a recorder, or of the SiteNode a node stream names.
    Records(IngestRecords),

    Drain(oneshot::Sender<()>),
}

/// One record batch handed to the store.
struct IngestRecords {
    records: Vec<MetricRecord>,

    /// The SiteNode whose stream sent the batch; none for a recorder's records.
    node: Option<String>,

    /// Whether the node sends the batch again after its stream failed or ended.
    backfill: bool,
}

/// Buckets one merged record batch changed, announced to watches.
type ChangeBatch = Arc<Vec<Change>>;

/// Buckets of one series a record landed in, as `(tier index, bucket start)`.
struct Change {
    key: SeriesKey,
    buckets: Vec<(usize, u64)>,
}

struct QueuedRecorder {
    ingest: mpsc::Sender<Ingest>,
    shared: Arc<Shared>,
}

impl MetricRecorder for QueuedRecorder {
    fn record(&self, records: Vec<MetricRecord>) {
        if records.is_empty() {
            return;
        }
        let count = records.len() as u64;
        if self
            .ingest
            .try_send(Ingest::Records(IngestRecords {
                records,
                node: None,
                backfill: false,
            }))
            .is_err()
        {
            self.shared
                .dropped_records
                .fetch_add(count, Ordering::Relaxed);
            debug!(
                event_kind = "metric_records_dropped",
                count, "metric store queue is full or stopped; records dropped"
            );
        }
    }
}

impl MetricIngest for QueuedRecorder {
    fn ingest<'a>(
        &'a self,
        node_id: &'a str,
        records: Vec<MetricRecord>,
        backfill: bool,
    ) -> BoxFuture<'a, Result<(), StoreStopped>> {
        Box::pin(async move {
            if records.is_empty() {
                return Ok(());
            }
            self.ingest
                .send(Ingest::Records(IngestRecords {
                    records,
                    node: Some(node_id.to_owned()),
                    backfill,
                }))
                .await
                .map_err(|_| StoreStopped)
        })
    }
}

async fn ingest_loop(
    shared: Arc<Shared>,
    mut receiver: mpsc::Receiver<Ingest>,
    changes: broadcast::Sender<ChangeBatch>,
    mut up: UpReadings,
) {
    let mut health = HealthCounts::default();
    let mut next_sweep = shared.clock.now() + shared.sweep_interval;
    loop {
        // Changes are collected only while someone watches.
        let mut changed = (changes.receiver_count() > 0).then(Vec::new);
        let next_bucket = next_finest_bucket(&shared);
        let wake = tokio::select! {
            message = receiver.recv() => Wake::Message(message),
            () = shared.clock.sleep_until(next_sweep) => Wake::Sweep,
            () = shared.clock.sleep_until(next_bucket) => Wake::Bucket,
        };
        // Every wake first marks the buckets elapsed since the last one, so idle stretches are
        // covered and a drain sees the bucket it runs in as observed.
        let readings = up.take_records(shared.clock.now(), shared.clock.wall_now());
        if !readings.is_empty() {
            let now = unix_millis(shared.clock.wall_now());
            let mut series = shared.series.lock().expect("metric store lock");
            merge_health_records(&shared, &mut series, now, readings, changed.as_mut());
        }
        match wake {
            Wake::Message(Some(Ingest::Records(batch))) => {
                merge_records(&shared, batch, &mut health, changed.as_mut());
            }
            Wake::Message(Some(Ingest::Drain(done))) => {
                let _ = done.send(());
            }
            // The store holds a sender for its whole life and aborts this task when dropped, so
            // the queue closes only while the store is going away.
            Wake::Message(None) => return,
            Wake::Sweep => {
                sweep(&shared, &mut health, changed.as_mut());
                next_sweep = shared.clock.now() + shared.sweep_interval;
            }
            Wake::Bucket => {}
        }
        if let Some(changed) = changed.filter(|changed| !changed.is_empty()) {
            // Sending fails only when every watch closed meanwhile.
            let _ = changes.send(Arc::new(changed));
        }
    }
}

/// Trims every series to retention, removes series left without history, and records the
/// store's health.
fn sweep(shared: &Shared, health: &mut HealthCounts, changed: Option<&mut Vec<Change>>) {
    let wall_now = shared.clock.wall_now();
    let now = unix_millis(wall_now);
    let tiers: Vec<(u64, u64)> = shared
        .retention
        .tiers()
        .iter()
        .map(|tier| {
            (
                width_millis(tier.width),
                now.saturating_sub(width_millis(tier.retention)),
            )
        })
        .collect();
    let mut series = shared.series.lock().expect("metric store lock");
    series.retain(|_, retained| {
        for (buckets, &(width, cutoff)) in retained.tiers.iter_mut().zip(&tiers) {
            trim(buckets, width, cutoff);
        }
        retained.tiers.iter().any(|buckets| !buckets.is_empty())
    });
    let bytes = series
        .iter()
        .map(|(key, retained)| estimated_bytes(key, retained))
        .sum();

    let records = health.take_records(
        wall_now,
        shared.dropped_records.load(Ordering::Relaxed),
        series.len(),
        bytes,
    );
    merge_health_records(shared, &mut series, now, records, changed);
}

/// Merges records the store measured about itself straight into history, bypassing the queue.
fn merge_health_records(
    shared: &Shared,
    series: &mut HashMap<SeriesKey, Series>,
    now: u64,
    records: Vec<MetricRecord>,
    mut changed: Option<&mut Vec<Change>>,
) {
    for record in records {
        // Health records are never counted as rejected, so a failure cannot feed itself.
        if let Err(rejection) =
            merge_record(shared, series, now, record, None, changed.as_deref_mut())
        {
            debug!(
                event_kind = "metric_health_record_rejected",
                metric = rejection.metric,
                reason = rejection.reason,
                "metric store health record rejected"
            );
        }
    }
}

/// Monotonic time at which the next finest-resolution bucket starts by the store's wall clock.
fn next_finest_bucket(shared: &Shared) -> Instant {
    let width = width_millis(shared.retention.tiers()[0].width);
    let now = unix_millis(shared.clock.wall_now());
    shared.clock.now() + Duration::from_millis(align_down(now, width) + width - now)
}

fn merge_records(
    shared: &Shared,
    batch: IngestRecords,
    health: &mut HealthCounts,
    mut changed: Option<&mut Vec<Change>>,
) {
    let now = unix_millis(shared.clock.wall_now());
    let node = batch.node.as_deref();
    let mut series = shared.series.lock().expect("metric store lock");
    for record in batch.records {
        let metric = record.metric.clone();
        match merge_record(
            shared,
            &mut series,
            now,
            record,
            node,
            changed.as_deref_mut(),
        ) {
            Ok(Merged::OnTime) => {}
            Ok(Merged::Late) => health.late(&metric, node, batch.backfill),
            Err(rejection) => {
                shared.rejected_records.fetch_add(1, Ordering::Relaxed);
                health.rejected(
                    &metric,
                    shared.descriptors.contains_key(&metric),
                    node,
                    rejection.reason,
                );
                debug!(
                    event_kind = "metric_record_rejected",
                    metric = rejection.metric,
                    reason = rejection.reason,
                    "metric record rejected"
                );
            }
        }
    }
}

fn merge_record(
    shared: &Shared,
    series: &mut HashMap<SeriesKey, Series>,
    now: u64,
    record: MetricRecord,
    node: Option<&str>,
    changed: Option<&mut Vec<Change>>,
) -> Result<Merged, Rejection> {
    let reject = |reason| Rejection {
        metric: record.metric.clone(),
        reason,
    };
    let descriptor = shared
        .descriptors
        .get(&record.metric)
        .ok_or_else(|| reject("unknown_metric"))?;
    descriptor
        .validate_labels(&record.labels)
        .map_err(|_| reject("invalid_labels"))?;
    // A node writes only the metrics it measures, and only for itself.
    if let Some(node) = node {
        if descriptor.provenance != MetricProvenance::Node {
            return Err(reject("not_node_measured"));
        }
        if record.labels.get(labels::NODE) != Some(node) {
            return Err(reject("other_node"));
        }
    }
    let (aggregate, source_width) = match (descriptor.kind, record.value) {
        (MetricKind::Gauge, RecordValue::Gauge(value)) => (
            Aggregate::Gauge(GaugeAggregate::from_sample(value, record.observed_at)),
            None,
        ),
        (MetricKind::Counter, RecordValue::Counter(delta)) => (
            Aggregate::Counter(CounterAggregate::from_delta(delta)),
            None,
        ),
        (MetricKind::Distribution { scale }, RecordValue::Distribution(value)) => {
            if !value.is_finite() || value < 0.0 {
                return Err(reject("invalid_value"));
            }
            (
                Aggregate::Distribution(DistributionAggregate::from_sample(value, scale)),
                None,
            )
        }
        (MetricKind::Gauge, RecordValue::Aggregate { width, aggregate })
            if matches!(aggregate, Aggregate::Gauge(_)) =>
        {
            (aggregate, Some(width_millis(width)))
        }
        (MetricKind::Counter, RecordValue::Aggregate { width, aggregate })
            if matches!(aggregate, Aggregate::Counter(_)) =>
        {
            (aggregate, Some(width_millis(width)))
        }
        (MetricKind::Distribution { .. }, RecordValue::Aggregate { width, aggregate })
            if matches!(aggregate, Aggregate::Distribution(_)) =>
        {
            (aggregate, Some(width_millis(width)))
        }
        _ => return Err(reject("kind_mismatch")),
    };
    if record.observed_at > to_system_time(now) + FUTURE_TOLERANCE {
        return Err(reject("future"));
    }

    let observed = unix_millis(record.observed_at);
    match source_width {
        Some(0) => return Err(reject("invalid_width")),
        // The store kept nothing from before it started, so such a bucket would merge partially.
        Some(width) if align_down(observed, width) < unix_millis(shared.incarnation.started_at) => {
            return Err(reject("before_store_start"));
        }
        _ => {}
    }
    let landing: Vec<(usize, u64, u64)> = shared
        .retention
        .tiers()
        .iter()
        .enumerate()
        .filter_map(|(index, tier)| {
            let width = width_millis(tier.width);
            if source_width.is_some_and(|source| !width.is_multiple_of(source)) {
                return None;
            }
            let cutoff = now.saturating_sub(width_millis(tier.retention));
            let bucket = align_down(observed, width);
            (bucket + width > cutoff).then_some((index, bucket, cutoff))
        })
        .collect();
    let Some(&(finest, finest_bucket, _)) = landing.first() else {
        return Err(reject("outside_retention"));
    };
    let finest_end = finest_bucket + width_millis(shared.retention.tiers()[finest].width);
    let merged = if finest_end + duration_millis(descriptor.closing_grace()) <= now {
        Merged::Late
    } else {
        Merged::OnTime
    };

    let tier_count = shared.retention.tiers().len();
    let key = SeriesKey {
        metric: record.metric.clone(),
        labels: record.labels.clone(),
    };
    if let Some(changed) = changed {
        changed.push(Change {
            key: key.clone(),
            buckets: landing
                .iter()
                .map(|&(index, bucket, _)| (index, bucket))
                .collect(),
        });
    }
    let entry = series.entry(key).or_insert_with(|| Series {
        tiers: vec![BTreeMap::new(); tier_count],
    });
    for (index, bucket, cutoff) in landing {
        let width = width_millis(shared.retention.tiers()[index].width);
        let buckets = &mut entry.tiers[index];
        match buckets.get_mut(&bucket) {
            Some(existing) => existing
                .merge(&aggregate)
                .map_err(|_| reject("kind_mismatch"))?,
            None => {
                buckets.insert(bucket, aggregate.clone());
            }
        }
        trim(buckets, width, cutoff);
    }
    Ok(merged)
}

/// Removes the buckets of width `width` that ended by `cutoff`.
fn trim(buckets: &mut BTreeMap<u64, Aggregate>, width: u64, cutoff: u64) {
    while let Some((&oldest, _)) = buckets.first_key_value()
        && oldest + width <= cutoff
    {
        buckets.pop_first();
    }
}

/// Approximate memory one series takes: its identity, and each bucket's start and aggregate,
/// including a distribution's histogram counts. Allocator and map node overhead are not counted.
fn estimated_bytes(key: &SeriesKey, series: &Series) -> usize {
    let identity = SERIES_OVERHEAD_BYTES
        + key.metric.len()
        + key
            .labels
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>();
    let buckets: usize = series
        .tiers
        .iter()
        .flat_map(|buckets| buckets.values())
        .map(|aggregate| {
            let histogram = match aggregate {
                Aggregate::Distribution(distribution) => {
                    std::mem::size_of_val(distribution.histogram().counts())
                }
                Aggregate::Gauge(_) | Aggregate::Counter(_) => 0,
            };
            std::mem::size_of::<u64>() + std::mem::size_of::<Aggregate>() + histogram
        })
        .sum();
    identity + buckets
}

fn to_system_time(millis: u64) -> SystemTime {
    crate::retention::from_unix_millis(millis)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What woke the ingest task.
enum Wake {
    Message(Option<Ingest>),
    Sweep,
    Bucket,
}

/// How a merged record arrived.
enum Merged {
    OnTime,

    /// After its finest bucket's end plus the metric's closing grace.
    Late,
}

struct Rejection {
    metric: String,
    reason: &'static str,
}

#[cfg(test)]
mod tests;
