//! Holds the timing policy of one process: how often its loops run, how long it coalesces, waits
//! between retries and treats something as still live.
//!
//! Composition hands one policy to every component, so a process runs either the production
//! schedule or, in tests, the same behavior compressed into milliseconds. Wall-aligned schedules
//! keep their alignment under every policy, and bounds on real I/O stay out of it.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Every behavior duration of one process, by the component that waits on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    /// Pause after a node's last control message before it sends a heartbeat.
    pub node_heartbeat_interval: Duration,

    /// Manager silence after which a node reopens its control stream.
    pub manager_silence_bound: Duration,

    /// Wall-aligned slot at which a node looks for a status change no signal announced.
    pub node_status_check_interval: Duration,

    /// Wall-aligned window in which a node's triggered statuses share one message.
    pub node_status_coalesce_window: Duration,

    /// First pause before a node reopens a control stream that failed or ended.
    pub node_stream_retry_backoff: Duration,

    /// Silence after which the site manager treats a node incarnation as no longer live.
    pub node_liveness_ttl: Duration,

    /// Pause between the site manager's publications of a node's unchanged view.
    pub view_republish_interval: Duration,

    /// Time a node's accepted view keeps permitting runtime actions after its latest
    /// publication.
    pub node_action_lease: Duration,

    /// Age after which a node treats its latest infrastructure collection as stale.
    pub target_capability_max_age: Duration,

    /// Wall-aligned slot of the management watches' ticks.
    pub watch_tick_interval: Duration,

    /// Delay after each watch slot starts before its tick reads, so readings of that slot arrive.
    pub watch_tick_grace: Duration,

    /// Pass schedule of the model observers.
    pub model_reconcile: ReconcileSchedule,

    /// Pass schedule of the application observers.
    pub application_reconcile: ReconcileSchedule,

    /// How long a native create watches a new workload for an early exit caused by a taken port.
    pub native_port_settle_window: Duration,

    /// Time between two steps of a simulated model's representative startup.
    pub simulated_startup_step: Duration,

    /// Bound on reaching a join listener and completing its handshake.
    pub join_connect_timeout: Duration,

    /// Bound on a join's commit round trip; past it the outcome is unknown.
    pub join_commit_timeout: Duration,

    /// Age after which the site manager reports a node's controller status as stale.
    pub node_controller_status_freshness: Duration,

    /// Longest pause before a node retries a failed model realizer construction; a change of its
    /// accepted view retries sooner.
    pub model_construction_retry: Duration,

    /// Least time between two stored download progress updates of one model preparation.
    pub download_progress_write_interval: Duration,

    /// Least time between two stored records of a node-hosted model that differ only in progress.
    pub node_progress_write_interval: Duration,

    /// Pause between two verifications of a healthy upstream endpoint, before its jitter.
    pub upstream_healthy_interval: Duration,

    /// First pause before a failed upstream verification runs again; it doubles up to the
    /// maximum.
    pub upstream_retry_initial: Duration,

    /// Longest pause before a failed upstream verification runs again.
    pub upstream_retry_max: Duration,

    /// How long an application deployment may boot before it no longer counts as starting.
    pub application_startup_grace: Duration,

    /// Longest a management watch stays silent; an idle watch then sends a heartbeat, so a
    /// vanished watcher's broken connection is noticed.
    pub watch_heartbeat_interval: Duration,

    /// How often the metric store trims expired history and records its own health.
    pub metric_store_sweep_interval: Duration,

    /// Pause before a failed pass over a node's Nix roots runs again, when no pin change triggers
    /// one sooner.
    pub nix_root_retry: Duration,

    /// Pause between keepalive pings on a private connection between the site manager and a
    /// worker.
    pub private_keepalive_interval: Duration,

    /// Time a private connection's keepalive ping may go unanswered before the connection counts
    /// as lost.
    pub private_keepalive_timeout: Duration,

    /// Time a terminal may take to answer a cursor-position query before it counts as one that
    /// never answers.
    pub terminal_cursor_query_timeout: Duration,
}

impl Timing {
    /// The schedule production backends run.
    pub const fn production() -> Self {
        Self {
            node_heartbeat_interval: Duration::from_secs(3),
            // Three missed publications at the production republish interval.
            manager_silence_bound: Duration::from_secs(60),
            node_status_check_interval: Duration::from_secs(1),
            node_status_coalesce_window: Duration::from_secs(1),
            node_stream_retry_backoff: Duration::from_millis(250),
            node_liveness_ttl: Duration::from_secs(10),
            view_republish_interval: Duration::from_secs(20),
            node_action_lease: Duration::from_secs(90),
            target_capability_max_age: Duration::from_secs(60),
            watch_tick_interval: Duration::from_secs(1),
            watch_tick_grace: Duration::from_millis(250),
            model_reconcile: ReconcileSchedule {
                pending_initial: Duration::from_millis(25),
                pending_max: Duration::from_millis(250),
                settled: Duration::from_secs(30),
            },
            application_reconcile: ReconcileSchedule {
                pending_initial: Duration::from_millis(250),
                pending_max: Duration::from_secs(2),
                settled: Duration::from_secs(30),
            },
            native_port_settle_window: Duration::from_secs(1),
            simulated_startup_step: Duration::from_millis(1_200),
            join_connect_timeout: Duration::from_secs(10),
            join_commit_timeout: Duration::from_secs(30),
            node_controller_status_freshness: Duration::from_secs(90),
            model_construction_retry: Duration::from_secs(5),
            download_progress_write_interval: Duration::from_secs(1),
            node_progress_write_interval: Duration::from_secs(2),
            upstream_healthy_interval: Duration::from_secs(60),
            upstream_retry_initial: Duration::from_secs(10),
            upstream_retry_max: Duration::from_secs(15 * 60),
            application_startup_grace: Duration::from_secs(5 * 60),
            watch_heartbeat_interval: Duration::from_secs(15),
            metric_store_sweep_interval: Duration::from_secs(10),
            nix_root_retry: Duration::from_secs(60),
            private_keepalive_interval: Duration::from_secs(10),
            private_keepalive_timeout: Duration::from_secs(5),
            terminal_cursor_query_timeout: Duration::from_secs(2),
        }
    }

    /// The policy this process runs: production, unless a test build was handed another through
    /// its environment.
    ///
    /// # Panics
    ///
    /// In a test build, when the handed policy cannot be decoded.
    pub fn for_process() -> Self {
        #[cfg(feature = "test-support")]
        if let Some(encoded) = std::env::var_os(TEST_TIMING_ENVIRONMENT) {
            let encoded = encoded.to_str().expect("the test timing policy is UTF-8");
            return serde_json::from_str(encoded).expect("decode the test timing policy");
        }
        Self::production()
    }

    /// The production schedule compressed for tests: the same behavior, in milliseconds where
    /// production waits seconds.
    ///
    /// Expiries stay long enough that a live node never lapses while a loaded host delays its
    /// heartbeats, the capability age keeps covering the site's hardware refresh, and the join
    /// bounds stay the production network bounds.
    #[cfg(any(test, feature = "test-support"))]
    pub const fn test() -> Self {
        let production = Self::production();
        Self {
            node_heartbeat_interval: Duration::from_millis(200),
            manager_silence_bound: Duration::from_secs(2),
            node_status_check_interval: Duration::from_millis(20),
            node_status_coalesce_window: Duration::from_millis(20),
            node_stream_retry_backoff: Duration::from_millis(20),
            node_liveness_ttl: Duration::from_secs(2),
            view_republish_interval: Duration::from_millis(500),
            node_action_lease: Duration::from_secs(5),
            target_capability_max_age: production.target_capability_max_age,
            watch_tick_interval: Duration::from_millis(100),
            watch_tick_grace: Duration::from_millis(20),
            model_reconcile: ReconcileSchedule {
                pending_initial: Duration::from_millis(5),
                pending_max: Duration::from_millis(50),
                settled: Duration::from_secs(1),
            },
            application_reconcile: ReconcileSchedule {
                pending_initial: Duration::from_millis(10),
                pending_max: Duration::from_millis(100),
                settled: Duration::from_secs(1),
            },
            native_port_settle_window: Duration::from_millis(50),
            simulated_startup_step: Duration::from_millis(10),
            join_connect_timeout: production.join_connect_timeout,
            join_commit_timeout: production.join_commit_timeout,
            // Stale reports, watch heartbeats, metric store health and application startup
            // failures would show in tests that idle for a moment; they keep production values.
            node_controller_status_freshness: production.node_controller_status_freshness,
            model_construction_retry: Duration::from_millis(100),
            download_progress_write_interval: Duration::from_millis(50),
            node_progress_write_interval: Duration::from_millis(100),
            upstream_healthy_interval: production.upstream_healthy_interval,
            upstream_retry_initial: Duration::from_millis(200),
            upstream_retry_max: Duration::from_secs(2),
            application_startup_grace: production.application_startup_grace,
            watch_heartbeat_interval: production.watch_heartbeat_interval,
            metric_store_sweep_interval: production.metric_store_sweep_interval,
            nix_root_retry: Duration::from_millis(500),
            // A lost private connection is noticed within two seconds; a loopback ping answers
            // far sooner even on a loaded host.
            private_keepalive_interval: Duration::from_secs(1),
            private_keepalive_timeout: Duration::from_secs(1),
            // Long enough for a test terminal that answers, short enough that one that never
            // answers costs little.
            terminal_cursor_query_timeout: Duration::from_millis(250),
        }
    }

    /// This policy encoded for [`TEST_TIMING_ENVIRONMENT`], which [`Self::for_process`] decodes.
    #[cfg(any(test, feature = "test-support"))]
    pub fn to_environment(&self) -> String {
        serde_json::to_string(self).expect("encode the timing policy")
    }
}

/// Environment variable through which a test hands a process it starts its timing policy.
#[cfg(any(test, feature = "test-support"))]
pub const TEST_TIMING_ENVIRONMENT: &str = "DOTCORE_TEST_TIMING";

impl Default for Timing {
    fn default() -> Self {
        Self::production()
    }
}

/// When an observer runs its next reconciliation pass: soon and backing off while work is
/// pending, at a fixed cadence once everything is settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileSchedule {
    /// First delay while work is pending; it doubles after each pending pass.
    pub pending_initial: Duration,

    /// Longest delay while work is pending.
    pub pending_max: Duration,

    /// Delay once every deployment is settled, which also bounds how long missed drift waits.
    pub settled: Duration,
}

/// Outcome of one reconciliation pass, which picks the delay before the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconciliationState {
    Pending,

    Settled,
}

/// One observer's position in its [`ReconcileSchedule`].
#[derive(Debug)]
pub struct ReconcileCadence {
    schedule: ReconcileSchedule,
    pending_delay: Duration,
}

impl ReconcileCadence {
    /// Starts at the schedule's first pending delay.
    pub fn new(schedule: ReconcileSchedule) -> Self {
        Self {
            schedule,
            pending_delay: schedule.pending_initial,
        }
    }

    /// Returns the delay after a pass that ended in `state`.
    pub fn next_delay(&mut self, state: ReconciliationState) -> Duration {
        match state {
            ReconciliationState::Pending => {
                let delay = self.pending_delay;
                self.pending_delay = self
                    .pending_delay
                    .saturating_mul(2)
                    .min(self.schedule.pending_max);
                delay
            }
            ReconciliationState::Settled => {
                self.reset();
                self.schedule.settled
            }
        }
    }

    /// Starts the pending back-off over, as after a change wakes the observer.
    pub fn reset(&mut self) {
        self.pending_delay = self.schedule.pending_initial;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn production_model_passes_back_off_to_the_maximum_and_a_settled_pass_starts_over() {
        let schedule = Timing::production().model_reconcile;
        let mut cadence = ReconcileCadence::new(schedule);

        let pending: Vec<_> = (0..6)
            .map(|_| cadence.next_delay(ReconciliationState::Pending))
            .collect();
        assert_eq!(
            pending,
            [25, 50, 100, 200, 250, 250].map(Duration::from_millis)
        );

        assert_eq!(
            cadence.next_delay(ReconciliationState::Settled),
            Duration::from_secs(30)
        );
        assert_eq!(
            cadence.next_delay(ReconciliationState::Pending),
            Duration::from_millis(25)
        );
    }

    #[test]
    fn production_application_passes_back_off_to_two_seconds_and_a_reset_starts_over() {
        let mut cadence = ReconcileCadence::new(Timing::production().application_reconcile);

        let pending: Vec<_> = (0..5)
            .map(|_| cadence.next_delay(ReconciliationState::Pending))
            .collect();
        assert_eq!(
            pending,
            [250, 500, 1_000, 2_000, 2_000].map(Duration::from_millis)
        );
        assert_eq!(
            cadence.next_delay(ReconciliationState::Settled),
            Duration::from_secs(30)
        );

        cadence.next_delay(ReconciliationState::Pending);
        cadence.reset();

        assert_eq!(
            cadence.next_delay(ReconciliationState::Pending),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn production_wall_aligned_slots_stay_whole_seconds() {
        let production = Timing::production();

        assert_eq!(
            production.node_status_check_interval,
            Duration::from_secs(1)
        );
        assert_eq!(
            production.node_status_coalesce_window,
            Duration::from_secs(1)
        );
        assert_eq!(production.watch_tick_interval, Duration::from_secs(1));
        assert_eq!(production.watch_tick_grace, Duration::from_millis(250));
    }

    #[test]
    fn the_default_policy_is_production() {
        assert_eq!(Timing::default(), Timing::production());
    }

    #[test]
    fn an_encoded_policy_decodes_to_the_same_policy() {
        let encoded = Timing::test().to_environment();

        assert_eq!(
            serde_json::from_str::<Timing>(&encoded).expect("decode"),
            Timing::test()
        );
    }

    #[test]
    fn the_test_policy_never_waits_longer_than_production() {
        let production = Timing::production();
        let test = Timing::test();
        let pairs = [
            (
                test.node_heartbeat_interval,
                production.node_heartbeat_interval,
            ),
            (test.manager_silence_bound, production.manager_silence_bound),
            (
                test.node_status_check_interval,
                production.node_status_check_interval,
            ),
            (
                test.node_status_coalesce_window,
                production.node_status_coalesce_window,
            ),
            (
                test.node_stream_retry_backoff,
                production.node_stream_retry_backoff,
            ),
            (test.node_liveness_ttl, production.node_liveness_ttl),
            (
                test.view_republish_interval,
                production.view_republish_interval,
            ),
            (test.node_action_lease, production.node_action_lease),
            (test.watch_tick_interval, production.watch_tick_interval),
            (test.watch_tick_grace, production.watch_tick_grace),
            (
                test.native_port_settle_window,
                production.native_port_settle_window,
            ),
            (
                test.simulated_startup_step,
                production.simulated_startup_step,
            ),
            (
                test.model_construction_retry,
                production.model_construction_retry,
            ),
            (
                test.download_progress_write_interval,
                production.download_progress_write_interval,
            ),
            (
                test.node_progress_write_interval,
                production.node_progress_write_interval,
            ),
            (
                test.upstream_retry_initial,
                production.upstream_retry_initial,
            ),
            (test.upstream_retry_max, production.upstream_retry_max),
            (test.nix_root_retry, production.nix_root_retry),
            (
                test.private_keepalive_interval,
                production.private_keepalive_interval,
            ),
            (
                test.private_keepalive_timeout,
                production.private_keepalive_timeout,
            ),
            (
                test.terminal_cursor_query_timeout,
                production.terminal_cursor_query_timeout,
            ),
        ];
        for (test, production) in pairs {
            assert!(test <= production, "{test:?} > {production:?}");
        }
        // A live node renews well within its liveness window and its manager's silence bound.
        assert!(test.node_heartbeat_interval * 5 <= test.node_liveness_ttl);
        assert!(test.view_republish_interval * 3 < test.manager_silence_bound);
        assert!(test.manager_silence_bound < test.node_action_lease);
    }
}
