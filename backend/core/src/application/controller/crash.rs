//! Apps-controller crash handling: what a deployment whose workload units stopped on their own
//! leads to, and the failure it records when it is not started again.
//!
//! A unit that stops before the deployment was ready in its start fails the deployment at once;
//! one that stops after it was ready restarts until the crash budget is spent. A failed
//! deployment keeps its group stopped, with the exit status and masked last output of the unit
//! that stopped, and is not started again in that start; pausing and resuming it, or deploying it
//! again, starts afresh. Stopped units this node never ran in the start, as after a node or host
//! restart, start as usual.

use std::{num::NonZeroU16, time::Duration};

use dotcore_domain::{
    deployment::crash::{CrashDecision, CrashFailure, crash_diagnostics},
    real_time,
};
use dotcore_proto::dotcore::application::v1::{
    ApplicationDeploymentDeclaration, ApplicationDeploymentFailure, ApplicationDeploymentState,
    ApplicationLifecycleState,
};
use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceSelection,
    WorkloadGroupLaunch, WorkloadUnitName, WorkloadUnitState,
};
use dotcore_site::view::fencing::LifecycleOperationToken;
use tracing::{debug, info, warn};

use super::{
    ApplicationRealizer, CurrentApplicationDeployment,
    errors::{DeploymentReconcileFailure, deployment_failure},
    failed_state,
    logs::deployment_protected_values,
    readiness::GROUP_NOT_RUNNING_STAGE,
};
use crate::management_logs::{MAX_FAILURE_OUTPUT_LINES, failure_output_tail};

/// Failure code of a deployment whose unit stopped before it was ready in its start.
const STOPPED_DURING_STARTUP: &str = "StoppedDuringStartup";

/// Failure code of a deployment whose units crashed too often after it was ready.
const CRASHED_REPEATEDLY: &str = "CrashedRepeatedly";

/// Bound on reading the last output of a unit that stopped, a real runtime read.
const OUTPUT_READ_TIMEOUT: Duration = Duration::from_secs(2);

impl ApplicationRealizer {
    /// Decides what the stopped units of a present deployment's group lead to, counting a crash
    /// after ready against its current start's budget.
    pub(super) fn decide_stop(
        &self,
        desired: &ApplicationDeploymentDeclaration,
        existing: Option<&CurrentApplicationDeployment>,
    ) -> CrashDecision {
        let recorded_ready = recorded_state_of_generation(desired, existing)
            .is_some_and(|state| state.lifecycle_state == ApplicationLifecycleState::Ready as i32);
        let decision = self.startup.decide_stop(
            &desired.application_deployment_id,
            desired.lifecycle_generation,
            recorded_ready,
            self.clock.now(),
        );
        if decision == CrashDecision::Restart {
            info!(
                event_kind = "application_crash_restart",
                deployment_id = %desired.application_deployment_id,
                lifecycle_generation = desired.lifecycle_generation,
                "Application units stopped after the deployment was ready; starting them again"
            );
        }
        decision
    }

    /// Records a crashed deployment as failed: the exit status and masked last output of its
    /// first stopped unit, read before anything removes it, and the rest of its group stopped so
    /// nothing of it keeps running.
    pub(super) async fn fail_crashed(
        &self,
        token: &LifecycleOperationToken,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
        group: &ObservedWorkloadGroup,
        failure: CrashFailure,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        let (unit_name, exit_status) = group
            .units
            .iter()
            .find_map(|(name, unit)| match unit.state {
                WorkloadUnitState::Stopped { exit_status } => Some((name, exit_status)),
                _ => None,
            })
            .expect("a crashed group has a stopped unit");
        let output = self
            .exited_unit_output(desired, execution_target_id, launch, unit_name)
            .await;
        if group.units.values().any(|unit| !unit.state.is_stopped()) {
            self.runtimes
                .stop_workload_group(token, execution_target_id, launch)
                .await
                .map_err(|source| deployment_failure("runtime-stop", source))?;
        }
        warn!(
            event_kind = "application_crash_failed",
            deployment_id = %desired.application_deployment_id,
            lifecycle_generation = desired.lifecycle_generation,
            unit_name = %unit_name.0,
            exit_status,
            failure = ?failure,
            "Application units stopped on their own; the deployment failed and is not started again"
        );
        Ok(failed_state(
            GROUP_NOT_RUNNING_STAGE,
            crash_failure(&failure, exit_status, &unit_name.0, output.as_deref()),
        ))
    }

    /// Reads the masked last output of one unit that stopped, `None` when the runtime has none
    /// or cannot read it in time.
    async fn exited_unit_output(
        &self,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
        unit_name: &WorkloadUnitName,
    ) -> Option<String> {
        let selection = RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
            name: unit_name.0.clone(),
            label: unit_name.0.clone(),
        }])
        .expect("one unit is a valid log selection");
        let lines = NonZeroU16::new(MAX_FAILURE_OUTPUT_LINES as u16).expect("positive line count");
        let query = RuntimeLogQuery::new(lines, None).expect("line count within runtime bounds");
        let read = real_time::bound(
            OUTPUT_READ_TIMEOUT,
            self.runtimes
                .read_workload_group_logs(execution_target_id, launch, &selection, &query),
        )
        .await;
        let history = match read {
            Ok(Ok(histories)) => histories
                .into_iter()
                .find_map(|history| history.result.ok()),
            Ok(Err(_)) | Err(_) => None,
        };
        if history.is_none() {
            debug!(
                event_kind = "application_crash_output_unavailable",
                deployment_id = %desired.application_deployment_id,
                unit_name = %unit_name.0,
                "Last output of the stopped application unit is unavailable"
            );
        }
        failure_output_tail(history?.entries, &deployment_protected_values(desired))
    }
}

/// The failed state a deployment recorded in its current lifecycle generation, whatever failed
/// it: a crash or a broken image. It stands until the deployment is paused and resumed or deployed
/// again, which starts a new generation.
pub(super) fn failed_state_of_generation<'a>(
    desired: &ApplicationDeploymentDeclaration,
    existing: Option<&'a CurrentApplicationDeployment>,
) -> Option<&'a ApplicationDeploymentState> {
    recorded_state_of_generation(desired, existing)
        .filter(|state| state.lifecycle_state == ApplicationLifecycleState::Failed as i32)
}

/// The state the node recorded for the deployment's current lifecycle generation, if any.
fn recorded_state_of_generation<'a>(
    desired: &ApplicationDeploymentDeclaration,
    existing: Option<&'a CurrentApplicationDeployment>,
) -> Option<&'a ApplicationDeploymentState> {
    existing
        .filter(|record| {
            record.reconciled_lifecycle_generation == Some(desired.lifecycle_generation)
        })
        .and_then(|record| record.state.as_ref())
}

/// The failure summary of a crashed deployment, naming the service whose unit stopped.
fn crash_failure(
    failure: &CrashFailure,
    exit_status: Option<i64>,
    service_name: &str,
    output: Option<&str>,
) -> ApplicationDeploymentFailure {
    let (code, stage, message) = match failure {
        CrashFailure::StoppedDuringStartup => (
            STOPPED_DURING_STARTUP,
            "application startup",
            "Application stopped during startup; check its logs, then pause and resume it or \
             deploy it again",
        ),
        CrashFailure::CrashBudgetSpent { .. } => (
            CRASHED_REPEATEDLY,
            "application running",
            "Application stopped repeatedly after it was ready; check its logs, then pause and \
             resume it or deploy it again",
        ),
    };
    ApplicationDeploymentFailure {
        code: code.to_owned(),
        message: message.to_owned(),
        diagnostics: crash_diagnostics(
            stage,
            failure,
            exit_status,
            &[format!("Service: {service_name}")],
            output,
        ),
    }
}
