//! Model-realizer crash handling: what a deployment whose engine stopped on its own leads to, and
//! the failure it records when the engine is not started again.
//!
//! An engine that stops before its deployment was ready in the lifecycle generation fails the
//! deployment at once. One that stops after the deployment was ready starts again until the crash
//! budget is spent; these restarts stay in the generation, so a crash while one loads counts
//! against the budget. A failure carries the exit status, the engine's own refusal and its masked
//! last output, and an engine of a ready deployment that this node never ran in the generation, as
//! after a node or host restart, starts as usual.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::Duration,
};

use dotcore_domain::{
    deployment::crash::{
        CrashDecision, CrashFailure, CrashHistory, StoppedWorkload, crash_diagnostics,
    },
    real_time,
};
use dotcore_proto::dotcore::model::v1::{
    ModelDeploymentDeclaration, ModelDeploymentFailure, ModelDeploymentState, ModelDeploymentStatus,
};
use dotcore_site::view::fencing::LifecycleOperationToken;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::{
    super::{
        InferenceControllerInner, ModelLifecycleError,
        state::{apply_crash_failure, engine_startup_failed_state, failed_state, startup_pending},
    },
    protected_startup_values,
};
use crate::{
    management_logs::{ProtectedLogValues, failure_output_lines, newest_failure_output},
    model::engine::startup_refusal::StartupRefusal,
};

/// Failure code of a deployment whose engine stopped too often after it was ready.
const ENGINE_CRASHED_REPEATEDLY: &str = "engine_crashed_repeatedly";

/// Bound on reading the output of an engine that stopped, a real runtime read.
const OUTPUT_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Tracks the current lifecycle generation of each deployment a realizer reconciles: whether this
/// node process ran its engine, whether the deployment was ready, and its crashes after ready.
/// Tracking lives in memory, so a node restart begins afresh.
pub(in crate::model::controller) struct CrashTracker {
    crash_window: Duration,
    starts: Mutex<BTreeMap<String, Start>>,
}

impl CrashTracker {
    pub(in crate::model::controller) fn new(crash_window: Duration) -> Self {
        Self {
            crash_window,
            starts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records that this node started the deployment's engine, or saw it running, in the
    /// lifecycle generation, so a later stop of it is a crash. A `retained` ready state of the
    /// generation is remembered as the deployment having been ready.
    pub(super) fn record_ran(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
        retained: Option<&ModelDeploymentState>,
    ) {
        let mut starts = self.lock();
        let start = current_start(&mut starts, deployment_id, lifecycle_generation);
        start.ran = true;
        start.ready |= retained_ready(retained);
    }

    /// Records that the deployment was ready in the lifecycle generation, so a later crash counts
    /// against its budget.
    pub(super) fn record_ready(&self, deployment_id: &str, lifecycle_generation: u64) {
        current_start(&mut self.lock(), deployment_id, lifecycle_generation).ready = true;
    }

    /// Decides what the stopped engine of a deployment leads to at `now`, recording a crash after
    /// ready against the generation's budget. The deployment's `retained` state of the generation
    /// stands in for what a restarted node process no longer remembers: a start still loading
    /// means this node ran the engine, and a ready one that the deployment was ready.
    pub(super) fn decide_stop(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
        retained: Option<&ModelDeploymentState>,
        now: Instant,
    ) -> CrashDecision {
        let mut starts = self.lock();
        let start = current_start(&mut starts, deployment_id, lifecycle_generation);
        start.ready |= retained_ready(retained);
        let stopped = StoppedWorkload {
            ran_in_generation: start.ran || retained.is_some_and(startup_pending),
            ready_in_generation: start.ready,
        };
        start.crashes.decide(stopped, now, self.crash_window)
    }

    /// Forgets deployments the realizer no longer reconciles.
    pub(super) fn retain(&self, deployment_ids: &BTreeSet<&str>) {
        self.lock()
            .retain(|deployment_id, _| deployment_ids.contains(deployment_id.as_str()));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Start>> {
        self.starts
            .lock()
            .unwrap_or_else(|value| value.into_inner())
    }
}

impl<D, S, R> InferenceControllerInner<D, S, R>
where
    D: super::super::ModelRealizationInput,
    S: super::super::CurrentStateRepository,
    R: super::super::ModelRealizationResults,
{
    /// Records that this node started the deployment's engine, or saw it running, in its current
    /// lifecycle generation, given its `retained` state of the generation.
    pub(super) fn record_engine_ran(
        &self,
        desired: &ModelDeploymentDeclaration,
        retained: Option<&ModelDeploymentState>,
    ) {
        self.crashes.record_ran(
            &desired.model_deployment_id,
            desired.lifecycle_generation,
            retained,
        );
    }

    /// Decides what the stopped engine of a present deployment leads to, given its `retained`
    /// state of the current lifecycle generation.
    pub(super) fn decide_stop(
        &self,
        desired: &ModelDeploymentDeclaration,
        retained: Option<&ModelDeploymentState>,
    ) -> CrashDecision {
        let decision = self.crashes.decide_stop(
            &desired.model_deployment_id,
            desired.lifecycle_generation,
            retained,
            self.clock.now(),
        );
        if decision == CrashDecision::Restart {
            info!(
                event_kind = "model_crash_restart",
                deployment_id = %desired.model_deployment_id,
                lifecycle_generation = desired.lifecycle_generation,
                "model engine stopped after the deployment was ready; starting it again",
            );
        }
        decision
    }

    /// Records a deployment whose engine crashed as failed, with the engine's exit status, refusal
    /// and masked last output. Returns whether it recorded the failure, which it does not when
    /// the deployment's retained state is missing or already failed.
    #[expect(
        clippy::too_many_arguments,
        reason = "the failure names the stopped workload, its launch and the pass's fencing"
    )]
    pub(super) async fn record_crash_failure(
        &self,
        desired: &ModelDeploymentDeclaration,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        launch: &dotcore_runtime::WorkloadLaunch,
        workload: &dotcore_runtime::ObservedWorkload,
        exit_status: Option<i64>,
        failure: CrashFailure,
        operation: &LifecycleOperationToken,
        binding: Option<&crate::inference::EngineBinding>,
    ) -> Result<bool, ModelLifecycleError> {
        let output = self.stopped_output(desired, target, workload, launch).await;
        let recorded = self.commit_status_operation(operation, binding, || {
            apply_crash_failure(&self.state, desired, |previous| {
                engine_crash_failed_state(previous, &failure, exit_status, &output)
            })
        })??;
        if recorded {
            let (error_kind, message) = match failure {
                CrashFailure::StoppedDuringStartup => (
                    "engine_startup_failed",
                    "model runtime workload stopped during startup",
                ),
                CrashFailure::CrashBudgetSpent { .. } => (
                    ENGINE_CRASHED_REPEATEDLY,
                    "model runtime workload stopped repeatedly after the deployment was ready",
                ),
            };
            warn!(
                event_kind = "model_lifecycle_problem",
                deployment_id = %desired.model_deployment_id,
                lifecycle_generation = desired.lifecycle_generation,
                error_kind,
                exit_status,
                "{message}",
            );
        }
        Ok(recorded)
    }

    /// Reads the newest page of a stopped engine's output for its failure, letting its engine
    /// recognize a refusal that the shown tail would cut off.
    async fn stopped_output(
        &self,
        desired: &ModelDeploymentDeclaration,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        workload: &dotcore_runtime::ObservedWorkload,
        launch: &dotcore_runtime::WorkloadLaunch,
    ) -> StoppedOutput {
        let source = dotcore_runtime::RuntimeLogSource {
            name: super::super::logs::MODEL_LOG_SOURCE_NAME.to_owned(),
            label: super::super::logs::MODEL_LOG_SOURCE_LABEL.to_owned(),
        };
        let history = dotcore_runtime::RuntimeLogQuery::new(
            std::num::NonZeroU16::new(dotcore_runtime::MAX_RUNTIME_LOG_ENTRIES)
                .expect("positive stopped output limit"),
            None,
        )
        .expect("stopped output limit is within runtime bounds");
        let query = dotcore_runtime::RuntimeWorkloadLogQuery::new(source, history);
        let protected = protected_startup_values(desired, &launch.spec.environment);
        match real_time::bound(
            OUTPUT_READ_TIMEOUT,
            self.runtimes.read_workload_logs(
                &target.execution_target_id,
                &workload.workload_id,
                &query,
            ),
        )
        .await
        {
            Ok(Ok(history)) => {
                let launch = super::super::launch::resolved_launch(desired).ok();
                stopped_output(history, &protected, &|output| {
                    let launch = launch.as_ref()?;
                    self.engine_integrations
                        .startup_refusal(&launch.snapshot, output)
                })
            }
            Ok(Err(_)) => {
                debug!(
                    event_kind = "model_startup_output_unavailable",
                    reason = "read_failed",
                    "stopped engine output unavailable"
                );
                StoppedOutput::default()
            }
            Err(_) => {
                debug!(
                    event_kind = "model_startup_output_unavailable",
                    reason = "timeout",
                    "stopped engine output timed out"
                );
                StoppedOutput::default()
            }
        }
    }
}

/// One deployment's current lifecycle generation on this node.
struct Start {
    lifecycle_generation: u64,

    /// Whether this node process started the engine, or saw it running, in the generation.
    ran: bool,

    /// Whether the deployment was ready in the generation.
    ready: bool,

    /// Crashes after ready in the generation; restarts within the budget keep the generation.
    crashes: CrashHistory,
}

impl Start {
    fn new(lifecycle_generation: u64) -> Self {
        Self {
            lifecycle_generation,
            ran: false,
            ready: false,
            crashes: CrashHistory::default(),
        }
    }
}

/// The deployment's start for `lifecycle_generation`, beginning a fresh one when the generation
/// changed.
fn current_start<'a>(
    starts: &'a mut BTreeMap<String, Start>,
    deployment_id: &str,
    lifecycle_generation: u64,
) -> &'a mut Start {
    let start = starts
        .entry(deployment_id.to_owned())
        .or_insert_with(|| Start::new(lifecycle_generation));
    if start.lifecycle_generation != lifecycle_generation {
        *start = Start::new(lifecycle_generation);
    }
    start
}

/// Whether the deployment's retained state shows it ready.
fn retained_ready(retained: Option<&ModelDeploymentState>) -> bool {
    retained.is_some_and(|state| {
        ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Ready)
    })
}

/// What a stopped engine's output tells its failure: the masked tail it shows and the engine's
/// own refusal, read from the whole page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StoppedOutput {
    tail: Option<String>,
    refusal: Option<StartupRefusal>,
}

/// Masks a stopped engine's output for its failure, keeps the newest lines as the tail, and reads
/// the engine's refusal from every line.
fn stopped_output(
    history: dotcore_runtime::RuntimeLogHistory,
    protected: &ProtectedLogValues,
    read_refusal: &dyn Fn(&str) -> Option<StartupRefusal>,
) -> StoppedOutput {
    let lines = failure_output_lines(history.entries, protected);
    StoppedOutput {
        refusal: read_refusal(&lines.join("\n")),
        tail: newest_failure_output(&lines),
    }
}

/// Failed state of a deployment whose engine crashed. A stop during startup takes its code and
/// explanation from the engine's refusal when it reported one; repeated crashes after ready have
/// their own.
fn engine_crash_failed_state(
    previous: &ModelDeploymentState,
    failure: &CrashFailure,
    exit_status: Option<i64>,
    output: &StoppedOutput,
) -> ModelDeploymentState {
    match failure {
        CrashFailure::StoppedDuringStartup => engine_startup_failed_state(
            previous,
            output.refusal.as_ref(),
            engine_crash_diagnostics("engine startup", failure, exit_status, output),
        ),
        CrashFailure::CrashBudgetSpent { .. } => failed_state(
            Some(previous),
            "Engine crashed repeatedly",
            ModelDeploymentFailure {
                code: ENGINE_CRASHED_REPEATEDLY.to_owned(),
                message: "Engine stopped repeatedly after the model was ready; check its logs, \
                          then pause and resume the model or deploy it again"
                    .to_owned(),
                diagnostics: engine_crash_diagnostics(
                    "engine running",
                    failure,
                    exit_status,
                    output,
                ),
            },
        ),
    }
}

/// Diagnostics of a crashed engine in the shared crash format, with the engine's refusal before
/// its output tail.
fn engine_crash_diagnostics(
    stage: &str,
    failure: &CrashFailure,
    exit_status: Option<i64>,
    output: &StoppedOutput,
) -> Vec<String> {
    let refusal = output
        .refusal
        .iter()
        .map(|refusal| format!("Engine refusal: {}", refusal.line()))
        .collect::<Vec<_>>();
    crash_diagnostics(
        stage,
        failure,
        exit_status,
        &refusal,
        output.tail.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use dotcore_domain::deployment::crash::CRASH_BUDGET;
    use dotcore_runtime::{
        RuntimeLogEntry, RuntimeLogHistory, RuntimeLogSource, RuntimeLogStreamKind,
    };

    use super::*;
    use crate::model::controller::state::loading_state;

    const WINDOW: Duration = Duration::from_secs(600);

    #[test]
    fn an_engine_the_node_never_ran_starts_even_when_the_deployment_was_ready() {
        let tracker = CrashTracker::new(WINDOW);

        assert_eq!(
            tracker.decide_stop("model", 1, Some(&ready_state()), Instant::now()),
            CrashDecision::Start
        );
    }

    #[test]
    fn a_retained_start_still_loading_fails_at_once_as_a_startup_crash() {
        let tracker = CrashTracker::new(WINDOW);

        assert_eq!(
            tracker.decide_stop("model", 1, Some(&loading_state()), Instant::now()),
            CrashDecision::Fail(CrashFailure::StoppedDuringStartup)
        );
    }

    #[test]
    fn crashes_after_ready_spend_the_budget_across_restarts_still_loading() {
        let tracker = CrashTracker::new(WINDOW);
        let now = Instant::now();
        tracker.record_ran("model", 1, None);
        tracker.record_ready("model", 1);

        // Each restart reads as a start still loading, which counts against the budget.
        assert_eq!(
            tracker.decide_stop("model", 1, Some(&loading_state()), now),
            CrashDecision::Restart
        );
        assert_eq!(
            tracker.decide_stop("model", 1, Some(&loading_state()), now),
            CrashDecision::Restart
        );
        assert_eq!(
            tracker.decide_stop("model", 1, Some(&loading_state()), now),
            CrashDecision::Fail(CrashFailure::CrashBudgetSpent {
                crashes: CRASH_BUDGET,
                window: WINDOW,
            })
        );
    }

    #[test]
    fn a_retained_ready_state_seen_while_running_counts_as_ready() {
        let tracker = CrashTracker::new(WINDOW);
        tracker.record_ran("model", 1, Some(&ready_state()));

        assert_eq!(
            tracker.decide_stop("model", 1, Some(&loading_state()), Instant::now()),
            CrashDecision::Restart
        );
    }

    #[test]
    fn a_new_lifecycle_generation_forgets_the_runs_and_crashes_of_the_previous_one() {
        let tracker = CrashTracker::new(WINDOW);
        let now = Instant::now();
        tracker.record_ran("model", 1, None);
        tracker.record_ready("model", 1);
        assert_eq!(
            tracker.decide_stop("model", 1, None, now),
            CrashDecision::Restart
        );

        assert_eq!(
            tracker.decide_stop("model", 2, None, now),
            CrashDecision::Start
        );
    }

    #[test]
    fn a_startup_crash_takes_the_engine_refusal_code_and_shows_its_line() {
        const LINE: &str = "ValueError: Free memory on device cuda:0 (111.66/121.63 GiB) on startup is less than desired GPU memory utilization (0.97, 117.98 GiB).";
        let output = StoppedOutput {
            tail: Some("RuntimeError: Engine core initialization failed".to_owned()),
            refusal: Some(StartupRefusal::InsufficientFreeMemory {
                needed_bytes: 126_680_060_396,
                free_bytes: 119_894_012_068,
                line: LINE.to_owned(),
            }),
        };

        let failed = engine_crash_failed_state(
            &loading_state(),
            &CrashFailure::StoppedDuringStartup,
            Some(1),
            &output,
        );

        assert_eq!(failed.status, ModelDeploymentStatus::Failed as i32);
        assert_eq!(failed.status_message, "Engine startup failed");
        let failure = failed.failure.expect("startup failure");
        assert_eq!(failure.code, "engine_insufficient_memory");
        assert_eq!(
            failure.diagnostics,
            [
                "Stage: engine startup".to_owned(),
                "Exit status: 1".to_owned(),
                format!("Engine refusal: {LINE}"),
                "Output tail: RuntimeError: Engine core initialization failed".to_owned(),
            ]
        );
    }

    #[test]
    fn repeated_crashes_after_ready_record_their_own_failure_with_the_crash_count() {
        let output = StoppedOutput {
            tail: Some("engine exited".to_owned()),
            refusal: None,
        };
        let mut ready = ready_state();
        ready.activation = Some(dotcore_proto::dotcore::model::v1::ModelDeploymentActivation {
            phase: dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::StartConfirmed
                as i32,
            workload_generation: Some(4),
            lifecycle_generation: 1,
        });

        let failed = engine_crash_failed_state(
            &ready,
            &CrashFailure::CrashBudgetSpent {
                crashes: CRASH_BUDGET,
                window: WINDOW,
            },
            None,
            &output,
        );

        assert_eq!(failed.status, ModelDeploymentStatus::Failed as i32);
        assert_eq!(failed.status_message, "Engine crashed repeatedly");
        assert_eq!(failed.activation, None);
        assert_eq!(
            failed.failure,
            Some(ModelDeploymentFailure {
                code: "engine_crashed_repeatedly".to_owned(),
                message: "Engine stopped repeatedly after the model was ready; check its logs, \
                          then pause and resume the model or deploy it again"
                    .to_owned(),
                diagnostics: vec![
                    "Stage: engine running".to_owned(),
                    "Crashes after ready: 3 within 10 minutes".to_owned(),
                    "Exit status: not reported".to_owned(),
                    "Output tail: engine exited".to_owned(),
                ],
            })
        );
    }

    #[test]
    fn stopped_output_masks_secrets_and_paths_and_its_diagnostics_keep_the_newest_bytes() {
        let history = log_history(vec![
            "booting".to_owned(),
            "File \"/opt/dotcore/models/snapshot/model_mtp.safetensors\" key=sk-test-private FileNotFoundError".to_owned(),
        ]);
        let output = stopped_output(
            history,
            &ProtectedLogValues::new(["sk-test-private".to_owned()]),
            &|_| None,
        );
        assert_eq!(
            output,
            StoppedOutput {
                tail: Some(
                    "booting\nFile \"[REDACTED]\" key=[REDACTED] FileNotFoundError".to_owned()
                ),
                refusal: None,
            }
        );

        // Line breaks inside one entry are dropped with the other control characters.
        let history = log_history(vec![format!("{}\nlast failure", "x".repeat(9000))]);
        let output = stopped_output(history, &ProtectedLogValues::default(), &|_| None);
        let diagnostics = engine_crash_diagnostics(
            "engine startup",
            &CrashFailure::StoppedDuringStartup,
            Some(17),
            &output,
        );
        assert_eq!(
            diagnostics,
            [
                "Stage: engine startup".to_owned(),
                "Exit status: 17".to_owned(),
                format!("Output tail: {}last failure", "x".repeat(6083)),
            ]
        );
        assert_eq!(
            diagnostics.iter().map(String::len).sum::<usize>(),
            dotcore_domain::deployment::MAX_FAILURE_DIAGNOSTIC_BYTES
        );
    }

    #[test]
    fn stopped_output_recognizes_an_engine_refusal_older_than_the_tail() {
        use dotcore_proto::dotcore::engine::v1::EngineKind;

        const REFUSAL: &str = "(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080] ValueError: Free memory on device cuda:0 (111.66/121.63 GiB) on startup is less than desired GPU memory utilization (0.97, 117.98 GiB). Decrease GPU memory utilization or reduce GPU memory used by other processes.";
        let later = (0..20)
            .map(|frame| format!("(APIServer pid=1)   frame {frame}, in run_engine_core"))
            .chain(["(APIServer pid=1) RuntimeError: Engine core initialization failed. See root cause above. Failed core proc(s): {}".to_owned()])
            .collect::<Vec<_>>();
        let history = log_history(
            std::iter::once(REFUSAL.to_owned())
                .chain(later.iter().cloned())
                .collect(),
        );
        let integrations = crate::model::controller::test_support::test_engine_integrations();
        let vllm = integrations
            .integration(EngineKind::Vllm)
            .expect("vLLM integration");

        let output = stopped_output(history, &ProtectedLogValues::default(), &|output| {
            vllm.startup_refusal(output)
        });

        assert_eq!(
            output,
            StoppedOutput {
                tail: Some(later[later.len() - 16..].join("\n")),
                refusal: Some(StartupRefusal::InsufficientFreeMemory {
                    needed_bytes: 126_680_060_396,
                    free_bytes: 119_894_012_068,
                    line: REFUSAL.to_owned(),
                }),
            }
        );
    }

    fn ready_state() -> ModelDeploymentState {
        let mut state = loading_state();
        state.status = ModelDeploymentStatus::Ready as i32;
        state.status_message = "Ready".to_owned();
        state
    }

    fn log_history(lines: Vec<String>) -> RuntimeLogHistory {
        RuntimeLogHistory {
            entries: lines
                .into_iter()
                .map(|line| RuntimeLogEntry {
                    source: RuntimeLogSource {
                        name: "model".to_owned(),
                        label: "Model".to_owned(),
                    },
                    timestamp: std::time::SystemTime::UNIX_EPOCH,
                    stream: RuntimeLogStreamKind::Stderr,
                    content: line.into_bytes(),
                    truncated: false,
                })
                .collect(),
            has_older_output: false,
            candidate_budget_truncated: false,
        }
    }
}
