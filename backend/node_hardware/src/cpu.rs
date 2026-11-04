//! Host CPU utilization sampling for node hardware collection.
//!
//! Utilization is the share of time the host's CPUs spent outside idle between two readings,
//! across all cores, from 0 to 1. A reading needs a previous one to compare with, so the first
//! sample reports nothing rather than a made-up value.

use std::sync::Mutex;

use sysinfo::{CpuRefreshKind, RefreshKind, System};

/// CPU utilization between consecutive samples, keeping the previous reading across calls.
pub(crate) struct CpuUtilization {
    state: Mutex<CpuState>,
}

impl CpuUtilization {
    /// Samples the host CPUs through sysinfo.
    pub(crate) fn for_current_host() -> Self {
        Self::with_reader(Box::new(SysinfoCpuReader(None)))
    }

    fn with_reader(reader: Box<dyn CpuUsageReader>) -> Self {
        Self {
            state: Mutex::new(CpuState {
                reader,
                primed: false,
            }),
        }
    }

    /// Returns the utilization since the previous call, or `None` on the first call and when the
    /// reading is not a number.
    pub(crate) fn sample(&self) -> Option<f64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let percent = state.reader.refresh();
        if !std::mem::replace(&mut state.primed, true) {
            return None;
        }

        Some((f64::from(percent?) / 100.0).clamp(0.0, 1.0)).filter(|ratio| !ratio.is_nan())
    }
}

struct CpuState {
    reader: Box<dyn CpuUsageReader>,

    /// Whether a previous reading exists to compare with.
    primed: bool,
}

/// Refreshes CPU times and reports the non-idle percentage since the previous refresh.
trait CpuUsageReader: Send {
    fn refresh(&mut self) -> Option<f32>;
}

/// Reads CPU times through one sysinfo system kept across refreshes, created on first use.
struct SysinfoCpuReader(Option<System>);

impl CpuUsageReader for SysinfoCpuReader {
    fn refresh(&mut self) -> Option<f32> {
        let system = self.0.get_or_insert_with(|| {
            System::new_with_specifics(
                RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
            )
        });
        system.refresh_cpu_usage();
        Some(system.global_cpu_usage())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[test]
    fn the_first_sample_reports_nothing_and_later_ones_a_ratio() {
        let cpu = CpuUtilization::with_reader(Box::new(FakeReader::new([
            Some(0.0),
            Some(37.5),
            Some(100.0),
        ])));

        assert_eq!(cpu.sample(), None);
        assert_eq!(cpu.sample(), Some(0.375));
        assert_eq!(cpu.sample(), Some(1.0));
    }

    #[test]
    fn out_of_range_and_unreadable_values_never_leave_zero_to_one() {
        let cpu = CpuUtilization::with_reader(Box::new(FakeReader::new([
            Some(0.0),
            Some(100.4),
            Some(-1.0),
            Some(f32::NAN),
            None,
        ])));

        assert_eq!(cpu.sample(), None);
        assert_eq!(cpu.sample(), Some(1.0));
        assert_eq!(cpu.sample(), Some(0.0));
        assert_eq!(cpu.sample(), None);
        assert_eq!(cpu.sample(), None);
    }

    #[test]
    fn the_host_reader_reports_a_ratio_on_its_second_sample() {
        let cpu = CpuUtilization::for_current_host();

        assert_eq!(cpu.sample(), None);
        let ratio = cpu.sample().expect("second sample");
        assert!((0.0..=1.0).contains(&ratio), "{ratio}");
    }

    struct FakeReader(VecDeque<Option<f32>>);

    impl FakeReader {
        fn new(readings: impl IntoIterator<Item = Option<f32>>) -> Self {
            Self(readings.into_iter().collect())
        }
    }

    impl CpuUsageReader for FakeReader {
        fn refresh(&mut self) -> Option<f32> {
            self.0.pop_front().expect("a configured reading")
        }
    }
}
