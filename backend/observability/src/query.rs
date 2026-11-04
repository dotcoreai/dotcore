//! Describes a read of one metric's history and its answer: aligned points per series group,
//! where a bucket without evidence is absent rather than zero.
//!
//! A query names the metric, label filters, the labels that keep series apart, how readings
//! combine within a bucket and across series, a time range, and a step hint. The store chooses the
//! resolution and states the effective step and covered range in the result.

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use crate::labels::LabelSet;

/// A read of one metric over a time range.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricQuery {
    /// Declared metric name.
    pub metric: String,

    /// Exact label values a series must carry to be read.
    pub matchers: BTreeMap<String, String>,

    /// Labels that keep series apart; series equal on these labels are combined.
    pub group_by: Vec<String>,

    /// How gauge readings combine within one bucket of one series. Required for gauges; counters
    /// always add and distributions always merge, so they ignore it.
    pub reduce: Option<Reduce>,

    /// How series in one group combine per bucket. Required for gauges and counters;
    /// distributions ignore it, since their histograms merge.
    pub aggregate: Option<SeriesAggregate>,

    /// Quantiles, from 0 to 1, estimated for each distribution point; ignored for other kinds.
    /// At most [`MAX_PERCENTILES`].
    pub percentiles: Vec<f64>,

    /// Time range to read.
    pub range: QueryRange,

    /// Desired point spacing; the store returns the closest spacing it can serve, never finer.
    pub step_hint: Duration,
}

/// Most percentiles one query may request, bounding the work per distribution point.
pub const MAX_PERCENTILES: usize = 16;

/// How gauge readings combine within one bucket of one series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reduce {
    /// Latest reading by observation time.
    Last,

    /// Mean reading.
    Avg,

    /// Smallest reading.
    Min,

    /// Largest reading.
    Max,

    /// Sum of readings.
    Sum,
}

/// How series of one group combine per bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesAggregate {
    /// Sum across series, such as total memory of every node.
    Sum,

    /// Mean across series.
    Avg,

    /// Smallest across series.
    Min,

    /// Largest across series.
    Max,
}

/// Time range of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRange {
    /// The most recent `Duration`, ending now.
    Last(Duration),

    /// From `start` (inclusive) to `end` (exclusive).
    Between { start: SystemTime, end: SystemTime },
}

/// Answer to one query.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Spacing between points: the bucket width served.
    pub effective_step: Duration,

    /// Aligned start of the first bucket the answer covers.
    pub covered_start: SystemTime,

    /// Aligned end of the last bucket the answer covers.
    pub covered_end: SystemTime,

    /// Store lifetime the answer comes from; a different one means held points are replaced, not
    /// merged.
    pub incarnation: StoreIncarnation,

    /// One series per group, ordered by labels.
    pub series: Vec<ResultSeries>,
}

/// Identity and start of one store lifetime. A new incarnation tells readers to replace the
/// points they hold rather than merge them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreIncarnation {
    /// Random identity of this lifetime.
    pub id: String,

    /// When this lifetime started.
    pub started_at: SystemTime,
}

/// Points of one series group.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSeries {
    /// The group's values of the query's group-by labels.
    pub labels: LabelSet,

    /// Points in time order; buckets without evidence are omitted.
    pub points: Vec<Point>,
}

/// Value of one bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// Aligned start of the bucket.
    pub bucket_start: SystemTime,

    /// Whether the bucket is still open and may change.
    pub provisional: bool,

    /// The bucket's value.
    pub value: PointValue,
}

/// Value of one bucket, by metric kind.
#[derive(Debug, Clone, PartialEq)]
pub enum PointValue {
    /// Combined gauge reading.
    Gauge(f64),

    /// Counter increments in the bucket and their rate per second over the step.
    Counter { delta: f64, rate_per_second: f64 },

    /// Summary of every distribution sample in the bucket.
    Distribution(DistributionPoint),
}

/// Summary of the distribution samples of one bucket across a series group.
#[derive(Debug, Clone, PartialEq)]
pub struct DistributionPoint {
    /// Number of samples.
    pub count: u64,

    /// Sum of samples.
    pub sum: f64,

    /// Smallest sample.
    pub min: f64,

    /// Largest sample.
    pub max: f64,

    /// Mean sample.
    pub mean: f64,

    /// Estimated sample at each requested quantile, as `(quantile, value)` in request order.
    pub percentiles: Vec<(f64, f64)>,
}

/// A query the store cannot answer.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum QueryError {
    #[error("metric {metric} is not declared")]
    UnknownMetric { metric: String },

    #[error("metric {metric} has no dimension {dimension}")]
    UnknownDimension { metric: String, dimension: String },

    #[error("metric query of {metric} needs a reduce, which gauges use")]
    MissingReduce { metric: String },

    #[error("metric query of {metric} needs an aggregate, which gauges and counters use")]
    MissingAggregate { metric: String },

    #[error("metric query range is empty or ends before it starts")]
    InvalidRange,

    #[error("metric query percentile {percentile} is not between 0 and 1")]
    InvalidPercentile { percentile: f64 },

    #[error("metric query requests {count} percentiles, above the limit of {limit}")]
    TooManyPercentiles { count: usize, limit: usize },

    #[error("metric query would return {points} points, above the limit of {limit}")]
    TooManyPoints { points: u64, limit: u64 },

    #[error("metric store is not running")]
    StoreStopped,
}
