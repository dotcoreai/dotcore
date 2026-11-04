//! Counts non-negative samples in exponentially sized buckets so a distribution can be summarized
//! in a few hundred bytes, merged exactly, and read back as estimated percentiles.
//!
//! Bucket boundaries are powers of `2^(2^-scale)`, the layout of Prometheus native histograms and
//! OpenTelemetry exponential histograms: at scale 3 each bucket is about 9% wider than the one
//! before it, so a percentile estimate is within about 4.4% of the true value. Histograms of
//! different scales merge by lowering the finer one to the coarser scale.

mod bounds;

use bounds::SCALE_BOUNDS;

/// Finest supported scale, as Prometheus native histograms.
pub const MAX_HISTOGRAM_SCALE: i8 = 8;

/// Coarsest supported scale, as Prometheus native histograms.
pub const MIN_HISTOGRAM_SCALE: i8 = -4;

/// Most buckets one histogram keeps; a wider spread of samples lowers the scale until it fits.
pub const MAX_HISTOGRAM_BUCKETS: usize = 160;

/// Sparse base-2 exponential histogram of non-negative samples.
///
/// Bucket `index` at scale `s` holds samples in `(2^((index-1)/2^s), 2^(index/2^s)]`, the
/// Prometheus native-histogram convention; zero and subnormal samples count in a separate zero
/// bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExponentialHistogram {
    scale: i8,
    zero_count: u64,
    offset: i32,
    counts: Vec<u64>,
}

impl ExponentialHistogram {
    /// An empty histogram at `scale`, clamped to the supported range.
    pub fn new(scale: i8) -> Self {
        Self {
            scale: scale.clamp(MIN_HISTOGRAM_SCALE, MAX_HISTOGRAM_SCALE),
            zero_count: 0,
            offset: 0,
            counts: Vec::new(),
        }
    }

    /// Counts one sample; `value` must be finite and non-negative.
    pub fn record(&mut self, value: f64) {
        if value < f64::MIN_POSITIVE {
            self.zero_count += 1;
            return;
        }
        let mut index = bucket_index(value, self.scale);
        while !self.fits(index, index) {
            self.downscale(1);
            index = bucket_index(value, self.scale);
        }
        self.add(index, 1);
    }

    /// Adds every sample of `other`, lowering either scale to the coarser one first.
    pub fn merge(&mut self, other: &Self) {
        let mut other = other.clone();
        let scale = self.scale.min(other.scale);
        self.downscale(self.scale - scale);
        other.downscale(other.scale - scale);
        while let Some((first, last)) = other.index_range()
            && !self.fits(first, last)
        {
            self.downscale(1);
            other.downscale(1);
        }
        self.zero_count += other.zero_count;
        for (position, count) in other.counts.iter().enumerate() {
            if *count > 0 {
                self.add(other.offset + position as i32, *count);
            }
        }
    }

    /// Scale of the bucket boundaries.
    pub fn scale(&self) -> i8 {
        self.scale
    }

    /// Samples counted in the zero bucket.
    pub fn zero_count(&self) -> u64 {
        self.zero_count
    }

    /// Index of the first bucket in [`Self::counts`].
    pub fn offset(&self) -> i32 {
        self.offset
    }

    /// Sample counts of consecutive buckets starting at [`Self::offset`].
    pub fn counts(&self) -> &[u64] {
        &self.counts
    }

    /// Total samples counted.
    pub fn count(&self) -> u64 {
        self.zero_count + self.counts.iter().sum::<u64>()
    }

    /// Bounds `(lower, upper]` of the bucket holding the sample of 1-based `rank` in ascending
    /// order; `None` for the zero bucket or a rank beyond the count.
    pub(crate) fn bucket_of_rank(&self, rank: u64) -> Option<(f64, f64)> {
        let mut seen = self.zero_count;
        if rank <= seen {
            return None;
        }
        for (position, count) in self.counts.iter().enumerate() {
            seen += count;
            if rank <= seen {
                return Some(bucket_bounds(self.offset + position as i32, self.scale));
            }
        }
        None
    }

    fn index_range(&self) -> Option<(i32, i32)> {
        let first = self.counts.iter().position(|count| *count > 0)?;
        let last = self.counts.iter().rposition(|count| *count > 0)?;
        Some((self.offset + first as i32, self.offset + last as i32))
    }

    /// Whether buckets `first..=last` fit beside the existing ones within the bucket limit.
    fn fits(&self, first: i32, last: i32) -> bool {
        let (low, high) = match self.index_range() {
            Some((low, high)) => (low.min(first), high.max(last)),
            None => (first, last),
        };
        ((high - low) as usize) < MAX_HISTOGRAM_BUCKETS
    }

    fn add(&mut self, index: i32, count: u64) {
        if self.counts.is_empty() {
            self.offset = index;
        }
        if index < self.offset {
            let grow = (self.offset - index) as usize;
            self.counts.splice(0..0, std::iter::repeat_n(0, grow));
            self.offset = index;
        }
        let position = (index - self.offset) as usize;
        if position >= self.counts.len() {
            self.counts.resize(position + 1, 0);
        }
        self.counts[position] += count;
    }

    /// Lowers the scale by `by`, merging each run of `2^by` buckets into one.
    fn downscale(&mut self, by: i8) {
        let by = by.min(self.scale - MIN_HISTOGRAM_SCALE);
        if by <= 0 {
            return;
        }
        let counts = std::mem::take(&mut self.counts);
        let offset = self.offset;
        self.scale -= by;
        for (position, count) in counts.into_iter().enumerate() {
            if count > 0 {
                self.add(coarser_index(offset + position as i32, by), count);
            }
        }
    }
}

/// Index of the bucket holding positive normal `value` at `scale`, computed exactly as Prometheus
/// native histograms do, so both agree on every boundary.
pub(crate) fn bucket_index(value: f64, scale: i8) -> i32 {
    let (fraction, exponent) = frexp(value);
    if scale > 0 {
        let bounds = SCALE_BOUNDS[scale as usize];
        bounds.partition_point(|bound| *bound < fraction) as i32
            + (exponent - 1) * bounds.len() as i32
    } else {
        let mut index = exponent;
        if fraction == 0.5 {
            index -= 1;
        }
        let offset = (1 << -scale) - 1;
        (index + offset) >> -scale
    }
}

/// Bounds `(lower, upper]` of bucket `index` at `scale`.
pub(crate) fn bucket_bounds(index: i32, scale: i8) -> (f64, f64) {
    let width = (-f64::from(scale)).exp2();
    (
        (f64::from(index - 1) * width).exp2(),
        (f64::from(index) * width).exp2(),
    )
}

/// Index at `scale - by` of the bucket containing bucket `index`: the ceiling of `index / 2^by`.
fn coarser_index(index: i32, by: i8) -> i32 {
    (index + (1 << by) - 1) >> by
}

/// Splits positive normal `value` into a fraction in `[0.5, 1)` and a power of two, as C `frexp`.
fn frexp(value: f64) -> (f64, i32) {
    const EXPONENT_MASK: u64 = 0x7ff << 52;
    let bits = value.to_bits();
    let exponent = ((bits & EXPONENT_MASK) >> 52) as i32 - 1022;
    let fraction = f64::from_bits((bits & !EXPONENT_MASK) | (1022 << 52));
    (fraction, exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scale_has_its_full_set_of_increasing_bounds() {
        for scale in 1..=MAX_HISTOGRAM_SCALE {
            let bounds = SCALE_BOUNDS[scale as usize];
            assert_eq!(bounds.len(), 1 << scale, "scale {scale}");
            assert_eq!(bounds[0], 0.5);
            assert!(
                bounds
                    .windows(2)
                    .all(|pair| pair[0] < pair[1] && pair[1] < 1.0)
            );
        }
    }

    #[test]
    fn bucket_indices_equal_prometheus_native_histogram_indices() {
        // Prometheus bucket `i` at schema `s` holds `(2^((i-1)/2^s), 2^(i/2^s)]`; powers of two
        // close a bucket rather than open one.
        let cases: [(f64, i8, i32); 14] = [
            (1.0, 3, 0),
            (2.0, 3, 8),
            (0.5, 3, -8),
            (1.05, 3, 1),
            (1.0905, 3, 1),
            (1.1, 3, 2),
            (0.25, 3, -16),
            (0.3, 3, -13),
            (1.0, 0, 0),
            (3.0, 0, 2),
            (4.0, 0, 2),
            (4.0, -1, 1),
            (5.0, -1, 2),
            (0.75, -2, 0),
        ];

        for (value, scale, expected) in cases {
            assert_eq!(bucket_index(value, scale), expected, "{value} at {scale}");
            let (lower, upper) = bucket_bounds(expected, scale);
            assert!(
                lower < value && value <= upper,
                "{value} in ({lower}, {upper}]"
            );
        }

        // Prometheus' bound for 2^-0.5 is one rounding step below the closest f64, which
        // therefore opens bucket 0 at scale 1 rather than closing bucket -1.
        assert_eq!(bucket_index(0.7071067811865475, 1), -1);
        assert_eq!(bucket_index(std::f64::consts::FRAC_1_SQRT_2, 1), 0);
    }

    #[test]
    fn downscaling_merges_whole_buckets() {
        let mut histogram = ExponentialHistogram::new(3);
        for value in [0.3, 0.9, 1.0, 1.05, 1.1, 2.0, 2.5] {
            histogram.record(value);
        }

        histogram.downscale(2);

        let mut expected = ExponentialHistogram::new(1);
        for value in [0.3, 0.9, 1.0, 1.05, 1.1, 2.0, 2.5] {
            expected.record(value);
        }
        assert_eq!(histogram, expected);
        assert_eq!(histogram.scale(), 1);
        assert_eq!(histogram.offset(), -3);
        assert_eq!(histogram.counts(), [1, 0, 0, 2, 2, 1, 1]);
    }

    #[test]
    fn merging_equals_recording_every_sample_including_across_scales() {
        let first = [0.0, 0.012, 0.25, 1.7];
        let second = [0.031, 0.25, 3.9, 12.0];
        let histogram = |scale: i8, values: &[f64]| {
            let mut histogram = ExponentialHistogram::new(scale);
            values.iter().for_each(|value| histogram.record(*value));
            histogram
        };

        let mut same_scale = histogram(3, &first);
        same_scale.merge(&histogram(3, &second));
        assert_eq!(same_scale, histogram(3, &[first, second].concat()));
        assert_eq!(same_scale.count(), 8);
        assert_eq!(same_scale.zero_count(), 1);

        let mut finer_into_coarser = histogram(2, &first);
        finer_into_coarser.merge(&histogram(3, &second));
        let mut coarser_into_finer = histogram(3, &first);
        coarser_into_finer.merge(&histogram(2, &second));
        assert_eq!(finer_into_coarser, histogram(2, &[first, second].concat()));
        assert_eq!(coarser_into_finer, histogram(2, &[first, second].concat()));
    }

    #[test]
    fn a_spread_wider_than_the_bucket_limit_lowers_the_scale() {
        let mut histogram = ExponentialHistogram::new(3);

        // 1ms to 1000s spans about 20 powers of two: 160 buckets at scale 3 just fit, so a sample
        // twice as large forces scale 2.
        histogram.record(0.001);
        histogram.record(1000.0);
        assert_eq!(histogram.scale(), 3);
        histogram.record(2000.0);

        assert_eq!(histogram.scale(), 2);
        assert_eq!(histogram.count(), 3);
        assert!(histogram.counts().len() <= MAX_HISTOGRAM_BUCKETS);
    }
}
