//! The in-memory store's own health, kept as metrics like any other: that it was running, how
//! much it holds, and the records it rejected, merged late, or dropped because its queue was full.
//!
//! Health records are written straight into the store's history by its ingest task, never through
//! the recorder queue, so a full queue cannot lose its own drop count. The store marks every
//! finest-resolution bucket it ran in with one up reading, which is the evidence that counters it
//! measures read zero rather than absent. A rejected record of an undeclared metric counts under
//! `metric = "unknown"`, so a writer sending arbitrary names cannot grow the number of series.

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use tokio::time::Instant;

use super::SWEEP_INTERVAL;
use crate::{
    descriptor::{MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricUnit},
    labels::{self, LabelSet},
    record::MetricRecord,
};

/// Whether the store was running: 1 once in every finest-resolution bucket it ran in, absent
/// otherwise.
pub(crate) const STORE_UP: &str = "observability.store.up";

/// Estimated memory of retained history.
pub(crate) const STORE_MEMORY: &str = "observability.store.memory";

/// Series holding retained history.
pub(crate) const STORE_SERIES: &str = "observability.store.series";

/// Records the store refused, by metric, sending node and reason.
pub(crate) const SAMPLES_REJECTED: &str = "observability.samples.rejected";

/// Records merged after their bucket closed, by metric, sending node, and whether a node sent
/// them again after a reconnect.
pub(crate) const SAMPLES_LATE: &str = "observability.samples.late";

/// Records dropped because the store's queue was full.
pub(crate) const RECORDER_DROPPED: &str = "observability.recorder.dropped";

/// Most an up reading lies past its bucket's start, which keeps a reading recorded as its bucket
/// starts within the store's tolerance for future observations.
const UP_READING_OFFSET: Duration = Duration::from_millis(500);

/// Label value standing for every undeclared metric name.
const UNKNOWN_METRIC: &str = "unknown";

/// Declarations of the metrics the store records about itself, with up readings every
/// `up_interval`.
pub(super) fn health_metric_descriptors(up_interval: Duration) -> Vec<MetricDescriptor> {
    let gauge = |name: &str, unit, interval| MetricDescriptor {
        name: name.to_owned(),
        kind: MetricKind::Gauge,
        unit,
        dimensions: vec![],
        provenance: MetricProvenance::Manager,
        sample_interval: Some(interval),
    };
    let counter = |name: &str, dimensions| MetricDescriptor {
        name: name.to_owned(),
        kind: MetricKind::Counter,
        unit: MetricUnit::Records,
        dimensions,
        provenance: MetricProvenance::Manager,
        sample_interval: None,
    };
    vec![
        gauge(STORE_UP, MetricUnit::Ratio, up_interval),
        gauge(STORE_MEMORY, MetricUnit::Bytes, SWEEP_INTERVAL),
        gauge(STORE_SERIES, MetricUnit::Series, SWEEP_INTERVAL),
        counter(
            SAMPLES_REJECTED,
            vec![
                MetricDimension::required(labels::METRIC),
                MetricDimension::optional(labels::NODE),
                MetricDimension::required(labels::REASON),
            ],
        ),
        counter(
            SAMPLES_LATE,
            vec![
                MetricDimension::required(labels::METRIC),
                MetricDimension::optional(labels::NODE),
                MetricDimension::required(labels::BACKFILL),
            ],
        ),
        counter(RECORDER_DROPPED, vec![]),
    ]
}

/// Counts the ingest task accumulates between sweeps.
#[derive(Default)]
pub(super) struct HealthCounts {
    rejected: BTreeMap<LabelSet, u64>,
    late: BTreeMap<LabelSet, u64>,

    /// Queue drops already written, from the store's running total.
    dropped_written: u64,
}

impl HealthCounts {
    /// Counts one rejected record of `metric`, which is `declared` or not, sent by `node`.
    pub(super) fn rejected(
        &mut self,
        metric: &str,
        declared: bool,
        node: Option<&str>,
        reason: &str,
    ) {
        let metric = if declared { metric } else { UNKNOWN_METRIC };
        let labels = with_node(LabelSet::new().with(labels::METRIC, metric), node)
            .with(labels::REASON, reason);
        *self.rejected.entry(labels).or_default() += 1;
    }

    /// Counts one record of `metric` merged after its bucket closed, sent by `node`.
    pub(super) fn late(&mut self, metric: &str, node: Option<&str>, backfill: bool) {
        let labels = with_node(LabelSet::new().with(labels::METRIC, metric), node)
            .with(labels::BACKFILL, backfill.to_string());
        *self.late.entry(labels).or_default() += 1;
    }

    /// Records, observed at `now`, of the counts since the previous call, the queue drops beyond
    /// those already written out of `dropped_total`, and the store's size.
    pub(super) fn take_records(
        &mut self,
        now: SystemTime,
        dropped_total: u64,
        series: usize,
        bytes: usize,
    ) -> Vec<MetricRecord> {
        let mut records = vec![
            MetricRecord::gauge(STORE_MEMORY, LabelSet::new(), now, bytes as f64),
            MetricRecord::gauge(STORE_SERIES, LabelSet::new(), now, series as f64),
        ];
        let counters = |metric: &'static str, counts: BTreeMap<LabelSet, u64>| {
            counts.into_iter().map(move |(labels, count)| {
                MetricRecord::counter(metric, labels, now, count as f64)
            })
        };
        records.extend(counters(
            SAMPLES_REJECTED,
            std::mem::take(&mut self.rejected),
        ));
        records.extend(counters(SAMPLES_LATE, std::mem::take(&mut self.late)));
        let dropped = dropped_total.saturating_sub(self.dropped_written);
        if dropped > 0 {
            records.push(MetricRecord::counter(
                RECORDER_DROPPED,
                LabelSet::new(),
                now,
                dropped as f64,
            ));
            self.dropped_written = dropped_total;
        }
        records
    }
}

/// The store's up readings: one per finest-resolution bucket since it started.
pub(super) struct UpReadings {
    /// Width of the finest resolution.
    width: Duration,

    /// Wall time of the latest reading.
    latest_wall: SystemTime,

    /// Monotonic time of the latest reading.
    latest_at: Instant,
}

impl UpReadings {
    /// Starts the readings of buckets `width` wide, returning the reading of the bucket holding
    /// `wall_now`, the store's start.
    pub(super) fn start(
        width: Duration,
        now: Instant,
        wall_now: SystemTime,
    ) -> (Self, MetricRecord) {
        let readings = Self {
            width,
            latest_wall: wall_now,
            latest_at: now,
        };
        let first = readings.reading(readings.bucket_index(wall_now));
        (readings, first)
    }

    /// Readings of the buckets elapsed since the latest one, up to the bucket holding `wall_now`.
    /// Monotonic time caps how many buckets count, so a suspended host or a wall clock jumping
    /// forward is not claimed as observed; the bucket holding `wall_now` always counts.
    pub(super) fn take_records(&mut self, now: Instant, wall_now: SystemTime) -> Vec<MetricRecord> {
        let latest = self.bucket_index(self.latest_wall);
        let current = self.bucket_index(wall_now);
        if current <= latest {
            return Vec::new();
        }
        // Buckets the latest reading's wall time would cross had wall time moved like monotonic
        // time.
        let observed = self
            .bucket_index(self.latest_wall + now.saturating_duration_since(self.latest_at))
            - latest;
        let count = (current - latest).min(observed.max(1));
        self.latest_wall = wall_now;
        self.latest_at = now;
        (current + 1 - count..=current)
            .map(|index| self.reading(index))
            .collect()
    }

    /// Index of the bucket holding `time`, counted from the Unix epoch.
    fn bucket_index(&self, time: SystemTime) -> u128 {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            / self.width.as_nanos()
    }

    /// One reading of bucket `index`, observed shortly after its start: a reading on a boundary
    /// would fall in the previous bucket for readers whose windows end at a boundary inclusively,
    /// and a reading far into a wide bucket would lie in the future when recorded at its start.
    fn reading(&self, index: u128) -> MetricRecord {
        let start =
            Duration::from_nanos(u64::try_from(index * self.width.as_nanos()).unwrap_or(u64::MAX));
        let offset = (self.width / 2).min(UP_READING_OFFSET);
        MetricRecord::gauge(
            STORE_UP,
            LabelSet::new(),
            SystemTime::UNIX_EPOCH + start + offset,
            1.0,
        )
    }
}

fn with_node(labels: LabelSet, node: Option<&str>) -> LabelSet {
    match node {
        Some(node) => labels.with(labels::NODE, node),
        None => labels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retention::unix_millis;

    #[test]
    fn up_readings_cover_elapsed_buckets_only_as_far_as_monotonic_time_moved() {
        let now = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_millis(1_000_250);
        let (mut up, first) = UpReadings::start(Duration::from_secs(1), now, wall);
        assert_eq!(observed_millis(&[first]), vec![1_000_500]);

        // Both clocks move three seconds.
        let records = up.take_records(now + Duration::from_secs(3), wall + Duration::from_secs(3));
        assert_eq!(
            observed_millis(&records),
            vec![1_001_500, 1_002_500, 1_003_500]
        );

        // The wall clock jumps an hour while monotonic time moves two seconds, as across a host
        // suspend: only the last two buckets count.
        let records = up.take_records(
            now + Duration::from_secs(5),
            wall + Duration::from_secs(3_603),
        );
        assert_eq!(observed_millis(&records), vec![4_602_500, 4_603_500]);

        // A wall clock stepping back records nothing until it passes the latest bucket again.
        let records = up.take_records(
            now + Duration::from_secs(6),
            wall + Duration::from_secs(3_000),
        );
        assert_eq!(observed_millis(&records), Vec::<u64>::new());
    }

    #[test]
    fn a_wake_stalled_across_two_boundaries_marks_both_buckets() {
        let now = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_micros(1_000_999_600);
        let (mut up, _) = UpReadings::start(Duration::from_secs(1), now, wall);

        // The next wake comes 1000.4 ms later, exactly on the second boundary after next.
        let records = up.take_records(
            now + Duration::from_micros(1_000_400),
            wall + Duration::from_micros(1_000_400),
        );

        assert_eq!(observed_millis(&records), vec![1_001_500, 1_002_500]);
    }

    #[test]
    fn a_wide_bucket_reading_lies_shortly_after_its_start() {
        let now = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(999_991);
        let (_, first) = UpReadings::start(Duration::from_secs(30), now, wall);

        // The bucket starts at 999990 s; its midpoint would lie 14 s past the store's start.
        assert_eq!(observed_millis(&[first]), vec![999_990_500]);
    }

    fn observed_millis(records: &[MetricRecord]) -> Vec<u64> {
        records
            .iter()
            .map(|record| unix_millis(record.observed_at))
            .collect()
    }
}
