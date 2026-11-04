//! Declares how much metric history the store keeps and at which resolutions, and aligns times to
//! bucket boundaries in Unix epoch time.
//!
//! The default keeps one-second buckets for 15 minutes, ten-second buckets for 6 hours, and
//! one-minute buckets for 48 hours. Every tier width is a multiple of the finer one, so a coarse
//! bucket covers whole fine buckets.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Resolutions and retention of metric history, finest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    tiers: Vec<RetentionTier>,
}

impl RetentionConfig {
    /// Validates `tiers`, finest first: whole-millisecond widths, each a multiple of the finer
    /// one, each retention a positive multiple of its width and no shorter than the finer one's.
    pub fn new(tiers: Vec<RetentionTier>) -> Result<Self, RetentionError> {
        if tiers.is_empty() {
            return Err(RetentionError::NoTiers);
        }
        for (index, tier) in tiers.iter().enumerate() {
            let width = tier.width.as_millis();
            if width == 0 || !tier.width.subsec_nanos().is_multiple_of(1_000_000) {
                return Err(RetentionError::InvalidWidth { width: tier.width });
            }
            let retention = tier.retention.as_millis();
            if retention == 0
                || !retention.is_multiple_of(width)
                || !tier.retention.subsec_nanos().is_multiple_of(1_000_000)
            {
                return Err(RetentionError::InvalidRetention {
                    width: tier.width,
                    retention: tier.retention,
                });
            }
            if let Some(finer) = index.checked_sub(1).map(|finer| &tiers[finer]) {
                if width <= finer.width.as_millis()
                    || !width.is_multiple_of(finer.width.as_millis())
                {
                    return Err(RetentionError::NotCoarser {
                        finer: finer.width,
                        coarser: tier.width,
                    });
                }
                if tier.retention < finer.retention {
                    return Err(RetentionError::ShorterThanFiner {
                        width: tier.width,
                        retention: tier.retention,
                    });
                }
            }
        }
        Ok(Self { tiers })
    }

    /// Tiers, finest first.
    pub fn tiers(&self) -> &[RetentionTier] {
        &self.tiers
    }

    /// Retention of the coarsest tier, the oldest history the store keeps.
    pub fn longest_retention(&self) -> Duration {
        self.tiers
            .last()
            .expect("retention has at least one tier")
            .retention
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self::new(vec![
            RetentionTier {
                width: Duration::from_secs(1),
                retention: Duration::from_secs(15 * 60),
            },
            RetentionTier {
                width: Duration::from_secs(10),
                retention: Duration::from_secs(6 * 60 * 60),
            },
            RetentionTier {
                width: Duration::from_secs(60),
                retention: Duration::from_secs(48 * 60 * 60),
            },
        ])
        .expect("default retention is valid")
    }
}

/// One resolution of metric history and how long it is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionTier {
    /// Bucket width.
    pub width: Duration,

    /// How far back buckets of this width are kept.
    pub retention: Duration,
}

/// Start of the epoch-aligned bucket of `width` containing `time`.
pub fn aligned_bucket_start(time: SystemTime, width: Duration) -> SystemTime {
    from_unix_millis(align_down(unix_millis(time), width_millis(width)))
}

/// Milliseconds since the Unix epoch; times before it read as the epoch.
pub(crate) fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn from_unix_millis(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

pub(crate) fn width_millis(width: Duration) -> u64 {
    (width.as_millis() as u64).max(1)
}

pub(crate) fn align_down(millis: u64, width: u64) -> u64 {
    millis - millis % width
}

pub(crate) fn align_up(millis: u64, width: u64) -> u64 {
    align_down(millis + width - 1, width)
}

/// Invalid retention tiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RetentionError {
    #[error("metric retention needs at least one tier")]
    NoTiers,

    #[error("metric retention tier width {width:?} is not a positive whole number of milliseconds")]
    InvalidWidth { width: Duration },

    #[error(
        "metric retention {retention:?} is not a positive multiple of its tier width {width:?}"
    )]
    InvalidRetention {
        width: Duration,
        retention: Duration,
    },

    #[error(
        "metric retention tier width {coarser:?} is not a larger multiple of the finer width {finer:?}"
    )]
    NotCoarser { finer: Duration, coarser: Duration },

    #[error(
        "metric retention {retention:?} of tier width {width:?} is shorter than a finer tier's"
    )]
    ShorterThanFiner {
        width: Duration,
        retention: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_align_to_epoch_multiples_of_their_width() {
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let time = base + Duration::from_millis(3_400);

        assert_eq!(
            aligned_bucket_start(time, Duration::from_secs(1)),
            base + Duration::from_secs(3)
        );
        assert_eq!(aligned_bucket_start(time, Duration::from_secs(10)), base);
        assert_eq!(
            aligned_bucket_start(time, Duration::from_secs(60)),
            UNIX_EPOCH + Duration::from_secs(1_699_999_980)
        );
    }

    #[test]
    fn a_time_on_a_boundary_starts_its_bucket() {
        let boundary = UNIX_EPOCH + Duration::from_secs(1_700_000_010);

        for width in [1, 10] {
            assert_eq!(
                aligned_bucket_start(boundary, Duration::from_secs(width)),
                boundary
            );
        }
        assert_eq!(
            aligned_bucket_start(boundary - Duration::from_millis(1), Duration::from_secs(10)),
            UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
    }

    #[test]
    fn the_default_retention_is_valid() {
        let tiers = RetentionConfig::default();

        assert_eq!(tiers.tiers().len(), 3);
        assert_eq!(tiers.longest_retention(), Duration::from_secs(48 * 60 * 60));
    }

    #[test]
    fn invalid_tiers_are_rejected() {
        let tier = |width, retention| RetentionTier {
            width: Duration::from_secs(width),
            retention: Duration::from_secs(retention),
        };

        assert_eq!(RetentionConfig::new(vec![]), Err(RetentionError::NoTiers));
        assert_eq!(
            RetentionConfig::new(vec![tier(10, 60), tier(15, 600)]),
            Err(RetentionError::NotCoarser {
                finer: Duration::from_secs(10),
                coarser: Duration::from_secs(15),
            })
        );
        assert_eq!(
            RetentionConfig::new(vec![tier(10, 60), tier(1, 600)]),
            Err(RetentionError::NotCoarser {
                finer: Duration::from_secs(10),
                coarser: Duration::from_secs(1),
            })
        );
        assert_eq!(
            RetentionConfig::new(vec![tier(10, 0)]),
            Err(RetentionError::InvalidRetention {
                width: Duration::from_secs(10),
                retention: Duration::ZERO,
            })
        );
        assert_eq!(
            RetentionConfig::new(vec![tier(1, 600), tier(10, 60)]),
            Err(RetentionError::ShorterThanFiner {
                width: Duration::from_secs(10),
                retention: Duration::from_secs(60),
            })
        );
    }
}
