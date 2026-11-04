//! Carries one measurement from a writer to the store: a raw gauge reading, counter increment or
//! distribution sample, or a summary a writer already aggregated over an aligned bucket.

use std::time::{Duration, SystemTime};

use crate::{aggregate::Aggregate, labels::LabelSet};

/// One measurement of one series.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricRecord {
    /// Declared metric name.
    pub metric: String,

    /// Labels identifying the series.
    pub labels: LabelSet,

    /// When the measurement was observed, by the measuring machine's wall clock. For a
    /// pre-aggregated value, any time inside its bucket.
    pub observed_at: SystemTime,

    /// The measured value.
    pub value: RecordValue,
}

impl MetricRecord {
    /// A gauge reading.
    pub fn gauge(
        metric: impl Into<String>,
        labels: LabelSet,
        observed_at: SystemTime,
        value: f64,
    ) -> Self {
        Self {
            metric: metric.into(),
            labels,
            observed_at,
            value: RecordValue::Gauge(value),
        }
    }

    /// A counter increment.
    pub fn counter(
        metric: impl Into<String>,
        labels: LabelSet,
        observed_at: SystemTime,
        delta: f64,
    ) -> Self {
        Self {
            metric: metric.into(),
            labels,
            observed_at,
            value: RecordValue::Counter(delta),
        }
    }

    /// A distribution sample, such as one request's latency; it must be finite and non-negative.
    pub fn distribution(
        metric: impl Into<String>,
        labels: LabelSet,
        observed_at: SystemTime,
        value: f64,
    ) -> Self {
        Self {
            metric: metric.into(),
            labels,
            observed_at,
            value: RecordValue::Distribution(value),
        }
    }
}

/// The value of one measurement.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordValue {
    /// One gauge reading.
    Gauge(f64),

    /// One counter increment.
    Counter(f64),

    /// One distribution sample.
    Distribution(f64),

    /// A summary of every sample in the epoch-aligned bucket of `width` containing the record's
    /// observation time; it lands in every retention tier whose width is a multiple of `width`.
    Aggregate {
        width: Duration,
        aggregate: Aggregate,
    },
}
