//! Shared crash rule for deployments whose workloads stop on their own.
//!
//! The hosting node asks it what to do when a workload it ran in the current lifecycle generation
//! is found stopped: a stop before the deployment was ready fails it at once, and a stop after it
//! was ready restarts it until too many such crashes fall within one crash window. It also builds
//! the crash failure diagnostics both controllers record. It reads time only from the instants its
//! caller passes, so tests decide it on a controlled clock.

use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;

use super::failure::failure_diagnostics;

/// Crashes after ready, within one crash window, that fail a deployment instead of restarting it.
pub const CRASH_BUDGET: u32 = 3;

/// Crashes after ready of one deployment in its current lifecycle generation, oldest first. The
/// hosting node keeps one per lifecycle generation, so pausing, resuming or redeploying starts a
/// new history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrashHistory {
    crashes: VecDeque<Instant>,
}

impl CrashHistory {
    /// Decides what a workload found stopped at `now` leads to, and records it when it is a crash
    /// after ready. Only crashes less than `window` before `now` count against the budget.
    pub fn decide(
        &mut self,
        stopped: StoppedWorkload,
        now: Instant,
        window: Duration,
    ) -> CrashDecision {
        if !stopped.ran_in_generation {
            return CrashDecision::Start;
        }
        if !stopped.ready_in_generation {
            return CrashDecision::Fail(CrashFailure::StoppedDuringStartup);
        }

        while self
            .crashes
            .front()
            .is_some_and(|crash| now.saturating_duration_since(*crash) >= window)
        {
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);

        let crashes = u32::try_from(self.crashes.len()).unwrap_or(u32::MAX);
        if crashes < CRASH_BUDGET {
            CrashDecision::Restart
        } else {
            CrashDecision::Fail(CrashFailure::CrashBudgetSpent { crashes, window })
        }
    }
}

/// What the hosting node knows about a workload it found stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoppedWorkload {
    /// Whether this node process saw the workload running, or started it, in the current
    /// lifecycle generation. A workload stopped by a node, runtime or host restart was not.
    pub ran_in_generation: bool,

    /// Whether the deployment was ready at some point in the current lifecycle generation.
    pub ready_in_generation: bool,
}

/// What the hosting node does with a workload it found stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashDecision {
    /// The stop is not a crash: start the workload as usual.
    Start,

    /// A crash after ready within the budget: start the workload again in the same lifecycle
    /// generation.
    Restart,

    /// Record the deployment as failed and leave the workload stopped.
    Fail(CrashFailure),
}

/// Why a crash fails a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashFailure {
    /// The workload stopped before the deployment was ready in this lifecycle generation.
    StoppedDuringStartup,

    /// The workload stopped `crashes` times after the deployment was ready, all within `window`.
    CrashBudgetSpent { crashes: u32, window: Duration },
}

/// Diagnostics of a crash failure, bounded like those of any failure: the stage, the crash count
/// after ready, the exit status (or that the runtime reported none), the owner's own `details` and
/// the masked output tail.
pub fn crash_diagnostics(
    stage: &str,
    failure: &CrashFailure,
    exit_status: Option<i64>,
    details: &[String],
    output_tail: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![format!("Stage: {stage}")];
    if let CrashFailure::CrashBudgetSpent { crashes, window } = failure {
        lines.push(format!(
            "Crashes after ready: {crashes} within {}",
            describe_duration(*window)
        ));
    }
    lines.push(match exit_status {
        Some(status) => format!("Exit status: {status}"),
        None => "Exit status: not reported".to_owned(),
    });
    lines.extend(details.iter().cloned());
    failure_diagnostics(lines, output_tail)
}

/// A crash window in operator words, in its largest whole unit.
fn describe_duration(duration: Duration) -> String {
    let (count, unit) = if duration.subsec_nanos() != 0 {
        (duration.as_millis(), "millisecond")
    } else if !duration.as_secs().is_multiple_of(60) {
        (u128::from(duration.as_secs()), "second")
    } else {
        (u128::from(duration.as_secs() / 60), "minute")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural}")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::clock::{Clock as _, VirtualClock};

    const WINDOW: Duration = Duration::from_secs(10 * 60);

    const BOOT_CRASH: StoppedWorkload = StoppedWorkload {
        ran_in_generation: true,
        ready_in_generation: false,
    };

    const READY_CRASH: StoppedWorkload = StoppedWorkload {
        ran_in_generation: true,
        ready_in_generation: true,
    };

    const NEVER_RAN: StoppedWorkload = StoppedWorkload {
        ran_in_generation: false,
        ready_in_generation: true,
    };

    #[test]
    fn a_stop_before_ready_fails_at_once_without_counting_a_crash() {
        let clock = clock();
        let mut history = CrashHistory::default();

        assert_eq!(
            history.decide(BOOT_CRASH, clock.now(), WINDOW),
            CrashDecision::Fail(CrashFailure::StoppedDuringStartup)
        );
        assert_eq!(history, CrashHistory::default());
    }

    #[test]
    fn crashes_after_ready_restart_until_the_budget_th_crash_in_the_window_fails() {
        let clock = clock();
        let mut history = CrashHistory::default();

        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Restart
        );
        clock.advance(WINDOW / 4);
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Restart
        );
        clock.advance(WINDOW / 4);
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Fail(CrashFailure::CrashBudgetSpent {
                crashes: CRASH_BUDGET,
                window: WINDOW,
            })
        );
    }

    #[test]
    fn crashes_spaced_beyond_the_window_keep_restarting() {
        let clock = clock();
        let mut history = CrashHistory::default();

        // A crash one window after the one before no longer counts it.
        for _ in 0..2 * CRASH_BUDGET {
            assert_eq!(
                history.decide(READY_CRASH, clock.now(), WINDOW),
                CrashDecision::Restart
            );
            clock.advance(WINDOW);
        }
    }

    #[test]
    fn a_crash_while_restarting_after_ready_counts_against_the_budget() {
        let clock = clock();
        let mut history = CrashHistory::default();
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Restart
        );

        // Restarts stay in the same lifecycle generation, which was ready, so stops before the
        // deployment is ready again are crashes after ready, not startup failures.
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Restart
        );
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Fail(CrashFailure::CrashBudgetSpent {
                crashes: CRASH_BUDGET,
                window: WINDOW,
            })
        );
    }

    #[test]
    fn a_stop_never_seen_running_starts_without_counting_a_crash() {
        let clock = clock();
        let mut history = CrashHistory::default();

        assert_eq!(
            history.decide(NEVER_RAN, clock.now(), WINDOW),
            CrashDecision::Start
        );
        assert_eq!(history, CrashHistory::default());

        // The full budget remains for later crashes.
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Restart
        );
        assert_eq!(
            history.decide(READY_CRASH, clock.now(), WINDOW),
            CrashDecision::Restart
        );
    }

    #[test]
    fn startup_failure_diagnostics_show_the_exit_status_details_and_output_tail() {
        assert_eq!(
            crash_diagnostics(
                "application startup",
                &CrashFailure::StoppedDuringStartup,
                Some(137),
                &["Unit: web".to_owned()],
                Some("listening\nout of memory"),
            ),
            [
                "Stage: application startup",
                "Exit status: 137",
                "Unit: web",
                "Output tail: listening\nout of memory",
            ]
        );
    }

    #[test]
    fn budget_failure_diagnostics_count_the_crashes_and_say_when_no_exit_status_was_reported() {
        let diagnostics = |window| {
            crash_diagnostics(
                "engine running",
                &CrashFailure::CrashBudgetSpent { crashes: 3, window },
                None,
                &[],
                None,
            )
        };

        assert_eq!(
            diagnostics(Duration::from_secs(10 * 60)),
            [
                "Stage: engine running",
                "Crashes after ready: 3 within 10 minutes",
                "Exit status: not reported",
            ]
        );
        assert_eq!(
            diagnostics(Duration::from_secs(60))[1],
            "Crashes after ready: 3 within 1 minute"
        );
        assert_eq!(
            diagnostics(Duration::from_secs(5))[1],
            "Crashes after ready: 3 within 5 seconds"
        );
        assert_eq!(
            diagnostics(Duration::from_millis(1_500))[1],
            "Crashes after ready: 3 within 1500 milliseconds"
        );
    }

    fn clock() -> VirtualClock {
        VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(1_800_000_000))
    }
}
