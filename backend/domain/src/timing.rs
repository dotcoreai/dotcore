//! Holds the timing policy of one process: how often its loops run, how long it coalesces, waits
//! between retries and treats something as still live.
//!
//! Composition hands each component its own part of one policy, so a process runs either the
//! production schedule or, in tests, the same behavior compressed into milliseconds. The policy
//! holds every behavior duration and the few real-time bounds a test needs to shorten; fixed
//! bounds on real I/O stay constants next to the code they bound.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Every behavior duration of one process, grouped by the component that waits on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    pub node_control: NodeControlTiming,
    pub watch: WatchTiming,
    pub model: ModelTiming,
    pub application: ApplicationTiming,
    pub metrics: MetricsTiming,
    pub artifact: ArtifactTiming,
    pub native: NativeTiming,
    pub simulated: SimulatedTiming,
    pub join: JoinTiming,
    pub private: PrivateTiming,
    pub terminal: TerminalTiming,
}

impl Timing {
    /// The schedule production backends run.
    pub const fn production() -> Self {
        Self {
            node_control: NodeControlTiming {
                heartbeat_interval: Duration::from_secs(3),
                manager_silence: Duration::from_secs(60),
                status_check_interval: Duration::from_secs(1),
                status_coalesce_window: Duration::from_secs(1),
                stream_retry: Backoff {
                    initial: Duration::from_millis(250),
                    max: Duration::from_secs(5),
                },
                node_silence: Duration::from_secs(10),
                view_republish_interval: Duration::from_secs(20),
                action_lease: Duration::from_secs(90),
                capability_max_age: Duration::from_secs(60),
                controller_status_max_age: Duration::from_secs(90),
            },
            watch: WatchTiming {
                tick_interval: Duration::from_secs(1),
                tick_read_delay: Duration::from_millis(250),
                heartbeat_interval: Duration::from_secs(15),
            },
            model: ModelTiming {
                reconcile: ReconcileSchedule {
                    pending_initial: Duration::from_millis(25),
                    pending_max: Duration::from_millis(250),
                    settled: Duration::from_secs(30),
                },
                construction_retry_interval: Duration::from_secs(5),
                download_progress_interval: Duration::from_secs(1),
                node_progress_interval: Duration::from_secs(2),
                upstream_healthy_interval: Duration::from_secs(60),
                upstream_retry: Backoff {
                    initial: Duration::from_secs(10),
                    max: Duration::from_secs(15 * 60),
                },
            },
            application: ApplicationTiming {
                reconcile: ReconcileSchedule {
                    pending_initial: Duration::from_millis(250),
                    pending_max: Duration::from_secs(2),
                    settled: Duration::from_secs(30),
                },
                startup_window: Duration::from_secs(5 * 60),
            },
            metrics: MetricsTiming {
                store_sweep_interval: Duration::from_secs(10),
                stream_silence: Duration::from_secs(30),
            },
            artifact: ArtifactTiming {
                nix_root_retry_interval: Duration::from_secs(60),
            },
            native: NativeTiming {
                port_settle_window: Duration::from_secs(1),
            },
            simulated: SimulatedTiming {
                startup_step_interval: Duration::from_millis(1_200),
                // The router then measures simulated generation as it measures an engine's.
                token_interval: Duration::from_millis(20),
            },
            join: JoinTiming {
                connect_timeout: Duration::from_secs(10),
                commit_timeout: Duration::from_secs(30),
            },
            private: PrivateTiming {
                keepalive_interval: Duration::from_secs(10),
                keepalive_timeout: Duration::from_secs(5),
            },
            terminal: TerminalTiming {
                cursor_query_timeout: Duration::from_secs(2),
            },
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

    /// Checks the relations between durations that the components rely on, so a policy that
    /// would let a live node lapse fails at composition instead of at run time.
    pub fn check(&self) -> Result<(), TimingError> {
        let control = &self.node_control;
        let schedule = [
            control.heartbeat_interval,
            control.manager_silence,
            control.status_check_interval,
            control.status_coalesce_window,
            control.stream_retry.initial,
        ];
        if schedule.iter().any(Duration::is_zero)
            || control.stream_retry.max < control.stream_retry.initial
        {
            return Err(TimingError::NodeControlSchedule);
        }
        if control.heartbeat_interval * HEARTBEATS_PER_SILENCE > control.node_silence {
            return Err(TimingError::NodeSilence);
        }
        if control.view_republish_interval * MISSED_PUBLICATIONS_BEFORE_REOPEN
            > control.manager_silence
        {
            return Err(TimingError::ManagerSilence);
        }
        let reopen = control.view_republish_interval * MISSED_PUBLICATIONS_BEFORE_REOPEN
            + control.stream_retry.max;
        if control.view_republish_interval.is_zero() || reopen >= control.action_lease {
            return Err(TimingError::ActionLease);
        }
        Ok(())
    }

    /// The production schedule compressed for tests: the same behavior, in milliseconds where
    /// production waits seconds.
    ///
    /// Expiries stay long enough that a live node never lapses while a loaded host delays its
    /// heartbeats, and the capability age, join bounds, stale-status age, watch heartbeats, metric
    /// store health, metric stream silence and application startup window keep their production
    /// values, which tests that idle for a moment would otherwise notice.
    #[cfg(any(test, feature = "test-support"))]
    pub const fn test() -> Self {
        let production = Self::production();
        Self {
            node_control: NodeControlTiming {
                heartbeat_interval: Duration::from_millis(200),
                manager_silence: Duration::from_secs(2),
                status_check_interval: Duration::from_millis(20),
                status_coalesce_window: Duration::from_millis(20),
                stream_retry: Backoff {
                    initial: Duration::from_millis(20),
                    max: Duration::from_millis(500),
                },
                node_silence: Duration::from_secs(2),
                view_republish_interval: Duration::from_millis(500),
                action_lease: Duration::from_secs(5),
                ..production.node_control
            },
            watch: WatchTiming {
                tick_interval: Duration::from_millis(100),
                tick_read_delay: Duration::from_millis(20),
                ..production.watch
            },
            model: ModelTiming {
                reconcile: ReconcileSchedule {
                    pending_initial: Duration::from_millis(5),
                    pending_max: Duration::from_millis(50),
                    settled: Duration::from_secs(1),
                },
                construction_retry_interval: Duration::from_millis(100),
                download_progress_interval: Duration::from_millis(50),
                node_progress_interval: Duration::from_millis(100),
                upstream_retry: Backoff {
                    initial: Duration::from_millis(200),
                    max: Duration::from_secs(2),
                },
                ..production.model
            },
            application: ApplicationTiming {
                reconcile: ReconcileSchedule {
                    pending_initial: Duration::from_millis(10),
                    pending_max: Duration::from_millis(100),
                    settled: Duration::from_secs(1),
                },
                ..production.application
            },
            metrics: production.metrics,
            artifact: ArtifactTiming {
                nix_root_retry_interval: Duration::from_millis(500),
            },
            native: NativeTiming {
                port_settle_window: Duration::from_millis(50),
            },
            simulated: SimulatedTiming {
                startup_step_interval: Duration::from_millis(10),
                // Streamed simulated answers arrive without pause.
                token_interval: Duration::ZERO,
            },
            join: production.join,
            // A lost private connection is noticed within moments; a loopback ping answers far
            // sooner even on a loaded host.
            private: PrivateTiming {
                keepalive_interval: Duration::from_secs(1),
                keepalive_timeout: Duration::from_secs(1),
            },
            // Long enough for a test terminal that answers, short enough that one that never
            // answers costs little.
            terminal: TerminalTiming {
                cursor_query_timeout: Duration::from_millis(250),
            },
        }
    }

    /// This policy encoded for [`TEST_TIMING_ENVIRONMENT`], which [`Self::for_process`] decodes.
    #[cfg(any(test, feature = "test-support"))]
    pub fn to_environment(&self) -> String {
        serde_json::to_string(self).expect("encode the timing policy")
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::production()
    }
}

/// Environment variable through which a test hands a process it starts its timing policy.
#[cfg(any(test, feature = "test-support"))]
pub const TEST_TIMING_ENVIRONMENT: &str = "DOTCORE_TEST_TIMING";

/// A relation between durations that a timing policy breaks.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimingError {
    #[error(
        "every node control interval and silence must be nonzero, and the longest retry pause at \
         least the first"
    )]
    NodeControlSchedule,

    #[error("a live node's heartbeats must fit several times within the node silence")]
    NodeSilence,

    #[error("a node must hear several view publications within the manager silence")]
    ManagerSilence,

    #[error(
        "a node that misses its view publications and reconnects after its longest retry pause \
         must receive a new view before its action lease expires"
    )]
    ActionLease,
}

/// Heartbeats a live node sends within the node silence, so missing all but one still keeps it
/// live.
const HEARTBEATS_PER_SILENCE: u32 = 3;

/// View publications a node misses before it reopens its control stream.
pub const MISSED_PUBLICATIONS_BEFORE_REOPEN: u32 = 3;

/// Timing of the control stream between the site manager and each node, on both of its ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeControlTiming {
    /// Pause after a node's last control message before it sends a heartbeat.
    pub heartbeat_interval: Duration,

    /// Manager silence after which a node reopens its control stream.
    pub manager_silence: Duration,

    /// Wall-aligned slot at which a node looks for a status change no signal announced.
    pub status_check_interval: Duration,

    /// Wall-aligned window in which a node's triggered statuses share one message.
    pub status_coalesce_window: Duration,

    /// Pauses before a node reopens a control stream that failed or ended.
    pub stream_retry: Backoff,

    /// Node silence after which the site manager treats a node incarnation as no longer live.
    pub node_silence: Duration,

    /// Pause between the site manager's publications of a node's unchanged view.
    pub view_republish_interval: Duration,

    /// Time a node's accepted view keeps permitting runtime actions after its latest
    /// publication.
    pub action_lease: Duration,

    /// Age after which a node's latest infrastructure collection counts as stale.
    pub capability_max_age: Duration,

    /// Age after which the site manager reports a node's controller status as stale.
    pub controller_status_max_age: Duration,
}

/// Timing of the management watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchTiming {
    /// Wall-aligned slot of the watches' ticks.
    pub tick_interval: Duration,

    /// Delay after each slot starts before its tick reads, so readings of that slot arrive.
    pub tick_read_delay: Duration,

    /// Longest a watch stays silent; an idle watch then sends a heartbeat, so a vanished
    /// watcher's broken connection is noticed.
    pub heartbeat_interval: Duration,
}

/// Timing of the model controller and its node-side realizers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTiming {
    /// Pass schedule of the model observers.
    pub reconcile: ReconcileSchedule,

    /// Longest pause before a node retries a failed model realizer construction; a change of its
    /// accepted view retries sooner.
    pub construction_retry_interval: Duration,

    /// Least time between two stored download progress updates of one model preparation.
    pub download_progress_interval: Duration,

    /// Least time between two stored records of a node-hosted model that differ only in progress.
    pub node_progress_interval: Duration,

    /// Pause between two verifications of a healthy upstream endpoint, before its jitter.
    pub upstream_healthy_interval: Duration,

    /// Pauses before a failed upstream verification runs again.
    pub upstream_retry: Backoff,
}

/// Timing of the application controller and its node-side realizers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationTiming {
    /// Pass schedule of the application observers.
    pub reconcile: ReconcileSchedule,

    /// How long an application deployment may boot before it no longer counts as starting.
    pub startup_window: Duration,
}

/// Timing of the metric store and the metric streams from nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsTiming {
    /// How often the metric store trims expired history and records its own health.
    pub store_sweep_interval: Duration,

    /// Silence after which the site manager ends a node's metric bucket stream.
    pub stream_silence: Duration,
}

/// Timing of the artifact manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactTiming {
    /// Pause before a failed pass over a node's Nix roots runs again, when no pin change triggers
    /// one sooner.
    pub nix_root_retry_interval: Duration,
}

/// Timing of the native runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTiming {
    /// How long a native create watches a new workload for an early exit caused by a taken port.
    pub port_settle_window: Duration,
}

/// Timing of simulated workloads and simulated inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedTiming {
    /// Time between two steps of a simulated model's representative startup.
    pub startup_step_interval: Duration,

    /// Pause between two streamed text tokens of a simulated model, its generation speed.
    pub token_interval: Duration,
}

/// Real-time bounds of joining a node to a site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinTiming {
    /// Bound on reaching a join listener and completing its handshake.
    pub connect_timeout: Duration,

    /// Bound on a join's commit round trip; past it the outcome is unknown.
    pub commit_timeout: Duration,
}

/// Real-time keepalive of the private connections between the site manager and its workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateTiming {
    /// Pause between keepalive pings on a private connection.
    pub keepalive_interval: Duration,

    /// Time a keepalive ping may go unanswered before the connection counts as lost.
    pub keepalive_timeout: Duration,
}

/// Real-time bounds of the command line's terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTiming {
    /// Time a terminal may take to answer a cursor-position query before it counts as one that
    /// never answers.
    pub cursor_query_timeout: Duration,
}

/// Pauses before a failed attempt runs again: the first pause, doubling after each failure up to
/// the longest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
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
pub enum ReconcileOutcome {
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

    /// Returns the delay after a pass that ended in `outcome`.
    pub fn next_delay(&mut self, outcome: ReconcileOutcome) -> Duration {
        match outcome {
            ReconcileOutcome::Pending => {
                let delay = self.pending_delay;
                self.pending_delay = self
                    .pending_delay
                    .saturating_mul(2)
                    .min(self.schedule.pending_max);
                delay
            }
            ReconcileOutcome::Settled => {
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

    use serde_json::Value;

    use super::*;

    #[test]
    fn production_model_passes_back_off_to_the_maximum_and_a_settled_pass_starts_over() {
        let mut cadence = ReconcileCadence::new(Timing::production().model.reconcile);

        let pending: Vec<_> = (0..6)
            .map(|_| cadence.next_delay(ReconcileOutcome::Pending))
            .collect();
        assert_eq!(
            pending,
            [25, 50, 100, 200, 250, 250].map(Duration::from_millis)
        );

        assert_eq!(
            cadence.next_delay(ReconcileOutcome::Settled),
            Duration::from_secs(30)
        );
        assert_eq!(
            cadence.next_delay(ReconcileOutcome::Pending),
            Duration::from_millis(25)
        );
    }

    #[test]
    fn production_application_passes_back_off_to_two_seconds_and_a_reset_starts_over() {
        let mut cadence = ReconcileCadence::new(Timing::production().application.reconcile);

        let pending: Vec<_> = (0..5)
            .map(|_| cadence.next_delay(ReconcileOutcome::Pending))
            .collect();
        assert_eq!(
            pending,
            [250, 500, 1_000, 2_000, 2_000].map(Duration::from_millis)
        );
        assert_eq!(
            cadence.next_delay(ReconcileOutcome::Settled),
            Duration::from_secs(30)
        );

        cadence.next_delay(ReconcileOutcome::Pending);
        cadence.reset();

        assert_eq!(
            cadence.next_delay(ReconcileOutcome::Pending),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn production_wall_aligned_slots_stay_whole_seconds() {
        let production = Timing::production();

        assert_eq!(
            production.node_control.status_check_interval,
            Duration::from_secs(1)
        );
        assert_eq!(
            production.node_control.status_coalesce_window,
            Duration::from_secs(1)
        );
        assert_eq!(production.watch.tick_interval, Duration::from_secs(1));
    }

    #[test]
    fn production_and_test_policies_keep_every_relation() {
        assert_eq!(Timing::production().check(), Ok(()));
        assert_eq!(Timing::test().check(), Ok(()));
    }

    #[test]
    fn a_zero_status_slot_or_a_retry_maximum_below_its_first_pause_is_rejected() {
        let mut zero_slot = Timing::test();
        zero_slot.node_control.status_check_interval = Duration::ZERO;
        let mut inverted_retry = Timing::test();
        inverted_retry.node_control.stream_retry.max = Duration::ZERO;

        assert_eq!(zero_slot.check(), Err(TimingError::NodeControlSchedule));
        assert_eq!(
            inverted_retry.check(),
            Err(TimingError::NodeControlSchedule)
        );
    }

    #[test]
    fn a_heartbeat_that_fills_the_node_silence_is_rejected() {
        let mut timing = Timing::test();
        timing.node_control.heartbeat_interval = timing.node_control.node_silence;

        assert_eq!(timing.check(), Err(TimingError::NodeSilence));
    }

    #[test]
    fn a_republish_interval_that_outlasts_the_manager_silence_is_rejected() {
        let mut timing = Timing::test();
        timing.node_control.view_republish_interval = timing.node_control.manager_silence;

        assert_eq!(timing.check(), Err(TimingError::ManagerSilence));
    }

    #[test]
    fn a_reconnect_that_outlasts_the_action_lease_is_rejected() {
        let mut timing = Timing::test();
        timing.node_control.stream_retry.max = timing.node_control.action_lease;

        assert_eq!(timing.check(), Err(TimingError::ActionLease));
    }

    #[test]
    fn the_test_policy_never_waits_longer_than_production() {
        let mut test = Vec::new();
        let mut production = Vec::new();
        durations(
            &serde_json::to_value(Timing::test()).expect("encode"),
            "",
            &mut test,
        );
        durations(
            &serde_json::to_value(Timing::production()).expect("encode"),
            "",
            &mut production,
        );

        assert_eq!(test.len(), production.len());
        for ((path, test), (_, production)) in test.into_iter().zip(production) {
            assert!(test <= production, "{path}: {test:?} > {production:?}");
        }
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

    /// Collects every duration of an encoded policy with its field path, in field order.
    fn durations(value: &Value, path: &str, found: &mut Vec<(String, Duration)>) {
        let Value::Object(fields) = value else {
            return;
        };
        if let (Some(secs), Some(nanos)) = (fields.get("secs"), fields.get("nanos")) {
            let secs = secs.as_u64().expect("whole seconds");
            let nanos = u32::try_from(nanos.as_u64().expect("nanoseconds")).expect("nanoseconds");
            found.push((path.to_owned(), Duration::new(secs, nanos)));
            return;
        }
        for (name, field) in fields {
            durations(field, &format!("{path}.{name}"), found);
        }
    }
}
