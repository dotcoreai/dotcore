//! Summarizes every sample that falls in one time bucket so buckets merge exactly: merging two
//! summaries equals summarizing all of their samples together.
//!
//! Exact merging lets the store write each sample into every retention tier at once, accept late
//! samples, and combine series across machines without re-reading raw samples.

use std::time::SystemTime;

use crate::histogram::ExponentialHistogram;

/// Summary of one bucket of one series, by metric kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Aggregate {
    /// Summary of gauge readings.
    Gauge(GaugeAggregate),

    /// Sum of counter increments.
    Counter(CounterAggregate),

    /// Summary and histogram of distribution samples.
    Distribution(DistributionAggregate),
}

impl Aggregate {
    /// Merges `other` into this summary; both must be of the same kind.
    pub fn merge(&mut self, other: &Self) -> Result<(), AggregateKindMismatch> {
        match (self, other) {
            (Self::Gauge(this), Self::Gauge(other)) => this.merge(other),
            (Self::Counter(this), Self::Counter(other)) => this.merge(other),
            (Self::Distribution(this), Self::Distribution(other)) => this.merge(other),
            _ => return Err(AggregateKindMismatch),
        }
        Ok(())
    }
}

/// Summary of gauge readings in one bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct GaugeAggregate {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    last: f64,
    last_observed_at: SystemTime,
}

impl GaugeAggregate {
    /// Summary of one reading of `value` observed at `observed_at`.
    pub fn from_sample(value: f64, observed_at: SystemTime) -> Self {
        Self {
            count: 1,
            sum: value,
            min: value,
            max: value,
            last: value,
            last_observed_at: observed_at,
        }
    }

    /// A summary another writer computed, such as one received from a node; `None` unless it
    /// counts at least one finite reading with `min <= last <= max`.
    pub fn from_parts(
        count: u64,
        sum: f64,
        min: f64,
        max: f64,
        last: f64,
        last_observed_at: SystemTime,
    ) -> Option<Self> {
        let finite = [sum, min, max, last].iter().all(|value| value.is_finite());
        (count > 0 && finite && min <= last && last <= max).then_some(Self {
            count,
            sum,
            min,
            max,
            last,
            last_observed_at,
        })
    }

    /// Merges `other`; the latest observation becomes the last reading, whatever the arrival
    /// order, and on equal observation times the merged-in reading wins.
    pub fn merge(&mut self, other: &Self) {
        self.count += other.count;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        if other.last_observed_at >= self.last_observed_at {
            self.last = other.last;
            self.last_observed_at = other.last_observed_at;
        }
    }

    /// Number of readings.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Sum of readings.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Smallest reading.
    pub fn min(&self) -> f64 {
        self.min
    }

    /// Largest reading.
    pub fn max(&self) -> f64 {
        self.max
    }

    /// Mean reading.
    pub fn avg(&self) -> f64 {
        self.sum / self.count as f64
    }

    /// Latest reading by observation time.
    pub fn last(&self) -> f64 {
        self.last
    }

    /// Observation time of the latest reading.
    pub fn last_observed_at(&self) -> SystemTime {
        self.last_observed_at
    }
}

/// Sum of counter increments in one bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CounterAggregate {
    delta: f64,
}

impl CounterAggregate {
    /// Summary of one increment.
    pub fn from_delta(delta: f64) -> Self {
        Self { delta }
    }

    /// Adds `other`'s increments.
    pub fn merge(&mut self, other: &Self) {
        self.delta += other.delta;
    }

    /// Total increment in the bucket.
    pub fn delta(&self) -> f64 {
        self.delta
    }
}

/// Summary of distribution samples in one bucket: exact count, sum, extremes, and an exponential
/// histogram from which percentiles are estimated.
#[derive(Debug, Clone, PartialEq)]
pub struct DistributionAggregate {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    histogram: ExponentialHistogram,
}

impl DistributionAggregate {
    /// Summary of one non-negative sample counted in a histogram of `scale`.
    pub fn from_sample(value: f64, scale: i8) -> Self {
        let mut histogram = ExponentialHistogram::new(scale);
        histogram.record(value);
        Self {
            count: 1,
            sum: value,
            min: value,
            max: value,
            histogram,
        }
    }

    /// Merges `other`; histograms of different scales merge at the coarser one.
    pub fn merge(&mut self, other: &Self) {
        self.count += other.count;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.histogram.merge(&other.histogram);
    }

    /// Number of samples.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Sum of samples.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Smallest sample.
    pub fn min(&self) -> f64 {
        self.min
    }

    /// Largest sample.
    pub fn max(&self) -> f64 {
        self.max
    }

    /// Mean sample.
    pub fn mean(&self) -> f64 {
        self.sum / self.count as f64
    }

    /// Bucket counts of the samples.
    pub fn histogram(&self) -> &ExponentialHistogram {
        &self.histogram
    }

    /// Estimate of the `quantile` (0 to 1) sample by nearest rank: the geometric midpoint of the
    /// histogram bucket holding that sample, kept within the smallest and largest sample.
    ///
    /// The estimate is within `2^(2^-scale / 2) - 1` of the true sample relative to it, about 4.4%
    /// at scale 3; zero-bucket samples read as zero.
    pub fn percentile(&self, quantile: f64) -> f64 {
        match self
            .histogram
            .bucket_of_rank(nearest_rank(quantile, self.count))
        {
            Some((lower, upper)) => (lower * upper).sqrt().clamp(self.min, self.max),
            None => 0.0,
        }
    }
}

/// 1-based rank of the `quantile` sample among `count` samples: `ceil(quantile × count)`, at least
/// 1. A product a rounding error above a whole number, such as `0.1 × 30`, stays at that number.
fn nearest_rank(quantile: f64, count: u64) -> u64 {
    const ROUNDING_TOLERANCE: f64 = 1e-9;
    ((quantile * count as f64 - ROUNDING_TOLERANCE).ceil() as u64).clamp(1, count)
}

/// Two summaries of different metric kinds cannot merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("metric aggregates of different kinds cannot merge")]
pub struct AggregateKindMismatch;

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn merging_gauge_summaries_equals_summarizing_all_readings() {
        let readings = [(7.0, 1), (3.0, 4), (9.0, 2), (5.0, 3)];
        let summary = |readings: &[(f64, u64)]| {
            let mut iter = readings
                .iter()
                .map(|(value, at)| GaugeAggregate::from_sample(*value, at_secs(*at)));
            let mut summary = iter.next().expect("one reading");
            iter.for_each(|reading| summary.merge(&reading));
            summary
        };

        let mut merged = summary(&readings[..2]);
        merged.merge(&summary(&readings[2..]));

        assert_eq!(merged, summary(&readings));
        assert_eq!(merged.count(), 4);
        assert_eq!(merged.sum(), 24.0);
        assert_eq!(merged.min(), 3.0);
        assert_eq!(merged.max(), 9.0);
        assert_eq!(merged.avg(), 6.0);
        assert_eq!(merged.last(), 3.0);
        assert_eq!(merged.last_observed_at(), at_secs(4));
    }

    #[test]
    fn a_gauge_summary_from_parts_must_be_consistent() {
        let at = at_secs(1);
        let summary = GaugeAggregate::from_parts(2, 8.0, 3.0, 5.0, 5.0, at).expect("valid");

        assert_eq!(summary.count(), 2);
        assert_eq!(summary.sum(), 8.0);
        assert_eq!(summary.min(), 3.0);
        assert_eq!(summary.max(), 5.0);
        assert_eq!(summary.last(), 5.0);
        assert_eq!(summary.last_observed_at(), at);
        for (count, sum, min, max, last) in [
            (0, 0.0, 0.0, 0.0, 0.0),
            (1, f64::NAN, 1.0, 1.0, 1.0),
            (1, 1.0, 2.0, 1.0, 1.0),
            (1, 1.0, 1.0, 2.0, 3.0),
        ] {
            assert_eq!(
                GaugeAggregate::from_parts(count, sum, min, max, last, at),
                None,
                "{count} {sum} {min} {max} {last}"
            );
        }
    }

    #[test]
    fn gauge_last_follows_observation_time_not_arrival() {
        let mut summary = GaugeAggregate::from_sample(10.0, at_secs(5));
        summary.merge(&GaugeAggregate::from_sample(20.0, at_secs(2)));

        assert_eq!(summary.last(), 10.0);
        assert_eq!(summary.last_observed_at(), at_secs(5));
    }

    #[test]
    fn merging_counter_summaries_adds_increments() {
        let mut first = Aggregate::Counter(CounterAggregate::from_delta(2.0));
        let second = Aggregate::Counter(CounterAggregate::from_delta(3.0));

        first.merge(&second).expect("same kind");

        assert_eq!(first, Aggregate::Counter(CounterAggregate::from_delta(5.0)));
    }

    #[test]
    fn merging_distribution_summaries_equals_summarizing_all_samples() {
        let summary = |samples: &[f64], scale: i8| {
            let mut iter = samples
                .iter()
                .map(|value| DistributionAggregate::from_sample(*value, scale));
            let mut summary = iter.next().expect("one sample");
            iter.for_each(|sample| summary.merge(&sample));
            summary
        };
        let samples = [0.5, 0.125, 2.0, 0.0, 4.5];

        let mut merged = summary(&samples[..2], 3);
        merged.merge(&summary(&samples[2..], 2));

        assert_eq!(merged, summary(&samples, 2));
        assert_eq!(merged.count(), 5);
        assert_eq!(merged.sum(), 7.125);
        assert_eq!(merged.min(), 0.0);
        assert_eq!(merged.max(), 4.5);
        assert_eq!(merged.mean(), 1.425);
        assert_eq!(merged.histogram().scale(), 2);
        assert_eq!(merged.histogram().zero_count(), 1);
    }

    #[test]
    fn percentile_estimates_stay_within_the_scale_relative_error() {
        // 1000 latencies spread geometrically from 10ms to about 11.5s.
        let samples: Vec<f64> = (0..1000)
            .map(|index| 0.01 * 1.00707_f64.powi(index))
            .collect();
        let mut summary = DistributionAggregate::from_sample(samples[0], 3);
        for sample in samples[1..].iter().rev() {
            summary.merge(&DistributionAggregate::from_sample(*sample, 3));
        }
        // Half a scale-3 bucket: 2^(1/16) - 1, about 4.4%.
        let relative_error = (1.0_f64 / 16.0).exp2() - 1.0;

        for quantile in [0.0, 0.25, 0.5, 0.9, 0.95, 0.99, 1.0] {
            let rank = ((quantile * 1000.0_f64).ceil() as usize).max(1);
            let truth = samples[rank - 1];
            let estimate = summary.percentile(quantile);
            assert!(
                (estimate - truth).abs() <= truth * relative_error,
                "p{quantile}: {estimate} vs {truth}"
            );
        }
        assert_eq!(summary.percentile(0.0), samples[0]);
        assert_eq!(summary.percentile(1.0), samples[999]);
    }

    #[test]
    fn the_nearest_rank_ignores_rounding_error_in_the_quantile_product() {
        assert_eq!(nearest_rank(0.1, 30), 3);
        assert_eq!(nearest_rank(0.07, 100), 7);
        assert_eq!(nearest_rank(0.5, 5), 3);
        assert_eq!(nearest_rank(0.95, 5), 5);
        assert_eq!(nearest_rank(0.0, 5), 1);
        assert_eq!(nearest_rank(1.0, 5), 5);
    }

    #[test]
    fn summaries_of_different_kinds_do_not_merge() {
        let mut gauge = Aggregate::Gauge(GaugeAggregate::from_sample(1.0, at_secs(1)));

        assert_eq!(
            gauge.merge(&Aggregate::Counter(CounterAggregate::from_delta(1.0))),
            Err(AggregateKindMismatch)
        );
    }

    fn at_secs(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + seconds)
    }
}
