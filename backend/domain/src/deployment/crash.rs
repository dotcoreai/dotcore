//! Shared crash rule for deployments whose workloads stop on their own.
//!
//! The hosting node asks it what to do when a workload it ran in the current lifecycle generation
//! is found stopped: a stop before the deployment was ready fails it at once, and a stop after it
//! was ready restarts it until too many such crashes fall within one crash window. It also builds
//! the failure diagnostics both controllers record, bounded to what a failure may carry. It reads
//! time only from the instants its caller passes, so tests decide it on a controlled clock.

use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;

use super::{MAX_FAILURE_DIAGNOSTIC_BYTES, MAX_FAILURE_DIAGNOSTICS};

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

/// Diagnostics of a crash failure, within the count and byte limits of a failure: the stage, the
/// crash count after ready, the exit status (or that the runtime reported none), the owner's own
/// `details` and the masked output tail. Details beyond the count limit are dropped, keeping a
/// line for the tail; beyond the byte limit, the tail loses its oldest output first.
pub fn crash_diagnostics(
    stage: &str,
    failure: &CrashFailure,
    exit_status: Option<i64>,
    details: &[String],
    output_tail: Option<&str>,
) -> Vec<String> {
    let output_tail = output_tail.filter(|tail| !tail.is_empty());

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
    lines.truncate(MAX_FAILURE_DIAGNOSTICS - usize::from(output_tail.is_some()));

    let mut remaining = MAX_FAILURE_DIAGNOSTIC_BYTES;
    let mut diagnostics = Vec::with_capacity(lines.len() + 1);
    for line in lines {
        if remaining == 0 {
            break;
        }
        let kept = leading(&line, remaining);
        remaining -= kept.len();
        diagnostics.push(kept.to_owned());
    }

    if let Some(tail) = output_tail
        && remaining > OUTPUT_TAIL_PREFIX.len()
    {
        let tail = trailing(tail, remaining - OUTPUT_TAIL_PREFIX.len());
        diagnostics.push(format!("{OUTPUT_TAIL_PREFIX}{tail}"));
    }
    diagnostics
}

const OUTPUT_TAIL_PREFIX: &str = "Output tail: ";

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

/// The longest start of `text` within `limit` bytes, cut on a character boundary.
fn leading(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The longest end of `text` within `limit` bytes, cut on a character boundary.
fn trailing(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
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

    #[test]
    fn diagnostics_beyond_the_count_limit_drop_details_and_keep_the_output_tail() {
        let details = (1..=10).map(|n| format!("Detail: {n}")).collect::<Vec<_>>();

        let diagnostics = crash_diagnostics(
            "application startup",
            &CrashFailure::StoppedDuringStartup,
            Some(1),
            &details,
            Some("last line"),
        );

        assert_eq!(
            diagnostics,
            [
                "Stage: application startup",
                "Exit status: 1",
                "Detail: 1",
                "Detail: 2",
                "Detail: 3",
                "Detail: 4",
                "Detail: 5",
                "Output tail: last line",
            ]
        );
        assert_eq!(diagnostics.len(), MAX_FAILURE_DIAGNOSTICS);
    }

    #[test]
    fn diagnostics_beyond_the_byte_limit_keep_the_end_of_the_output_tail() {
        let tail = format!(
            "{}{}",
            "a".repeat(10),
            "z".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES)
        );

        let diagnostics = crash_diagnostics(
            "application startup",
            &CrashFailure::StoppedDuringStartup,
            Some(1),
            &[],
            Some(&tail),
        );

        let fixed = "Stage: application startup".len() + "Exit status: 1".len();
        let room = MAX_FAILURE_DIAGNOSTIC_BYTES - fixed - OUTPUT_TAIL_PREFIX.len();
        assert_eq!(
            diagnostics,
            [
                "Stage: application startup".to_owned(),
                "Exit status: 1".to_owned(),
                format!("Output tail: {}", "z".repeat(room)),
            ]
        );
        assert_eq!(
            diagnostics.iter().map(String::len).sum::<usize>(),
            MAX_FAILURE_DIAGNOSTIC_BYTES
        );

        // A multibyte character straddling the cut moves it to the next character boundary.
        let straddling = format!("é{}", "z".repeat(room - 1));
        let diagnostics = crash_diagnostics(
            "application startup",
            &CrashFailure::StoppedDuringStartup,
            Some(1),
            &[],
            Some(&straddling),
        );
        assert_eq!(
            diagnostics[2],
            format!("Output tail: {}", "z".repeat(room - 1))
        );
    }

    #[test]
    fn details_beyond_the_byte_limit_are_cut_and_leave_no_room_for_the_tail() {
        let details = ["d".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES)];

        let diagnostics = crash_diagnostics(
            "application startup",
            &CrashFailure::StoppedDuringStartup,
            Some(1),
            &details,
            Some("last line"),
        );

        let fixed = "Stage: application startup".len() + "Exit status: 1".len();
        assert_eq!(
            diagnostics,
            [
                "Stage: application startup".to_owned(),
                "Exit status: 1".to_owned(),
                "d".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES - fixed),
            ]
        );
    }

    fn clock() -> VirtualClock {
        VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(1_800_000_000))
    }
}
