//! Apps-controller state values and repository error mappings.

use dotcore_domain::real_time;
use dotcore_proto::dotcore::application::v1::{
    ApplicationDeploymentStep, ApplicationDeploymentSteps,
};
use dotcore_runtime::{ObservedWorkloadGroup, WorkloadGroupLaunch};

use super::readiness::{
    ENDPOINT_NOT_ASSIGNED_STAGE, ENDPOINT_NOT_READY_STAGE, GROUP_NOT_RUNNING_STAGE,
};
use super::*;

pub(super) fn queued_state() -> ApplicationDeploymentState {
    state(
        ApplicationLifecycleState::Queued,
        ApplicationReadinessState::NotReady,
        "queued",
        None,
        Vec::new(),
    )
}

pub(super) fn stopping_state() -> ApplicationDeploymentState {
    state(
        ApplicationLifecycleState::Stopping,
        ApplicationReadinessState::NotReady,
        "stopping",
        None,
        Vec::new(),
    )
}

pub(super) fn starting_state(
    previous: Option<&ApplicationDeploymentState>,
) -> ApplicationDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(queued_state);
    state.lifecycle_state = ApplicationLifecycleState::Starting as i32;
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.stage = "starting".to_owned();
    state.services.clear();
    state.endpoint = None;
    state.updated_at = Some(now());
    state
}

/// State recorded while a starting deployment's image is prepared on its node. It keeps the
/// deployment's history but no failure, readiness, services or endpoint of an earlier start.
pub(super) fn preparing_state(
    previous: Option<&ApplicationDeploymentState>,
) -> ApplicationDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(queued_state);
    state.lifecycle_state = ApplicationLifecycleState::Preparing as i32;
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.stage = "preparing".to_owned();
    state.failure = None;
    state.services.clear();
    state.endpoint = None;
    state.updated_at = Some(now());
    state
}

/// Rebuilds a booting deployment's state as `Starting`. It keeps the stage, which names what the
/// deployment waits for, but publishes no endpoint until the deployment is ready.
pub(super) fn booting_state(mut state: ApplicationDeploymentState) -> ApplicationDeploymentState {
    state.lifecycle_state = ApplicationLifecycleState::Starting as i32;
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.services.clear();
    state.endpoint = None;
    state
}

pub(super) fn pausing_state(
    previous: Option<&ApplicationDeploymentState>,
) -> ApplicationDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(queued_state);
    state.lifecycle_state = ApplicationLifecycleState::Pausing as i32;
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.stage = "pausing".to_owned();
    state.services.clear();
    state.endpoint = None;
    state.updated_at = Some(now());
    state
}

pub(super) fn paused_state(
    previous: Option<&ApplicationDeploymentState>,
) -> ApplicationDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(pausing_state_default);
    state.lifecycle_state = ApplicationLifecycleState::Paused as i32;
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.stage = "paused".to_owned();
    state.services.clear();
    state.endpoint = None;
    state.updated_at = Some(now());
    state
}

fn pausing_state_default() -> ApplicationDeploymentState {
    pausing_state(None)
}

pub(super) fn stopped_state() -> ApplicationDeploymentState {
    state(
        ApplicationLifecycleState::Stopped,
        ApplicationReadinessState::NotReady,
        "stopped",
        None,
        Vec::new(),
    )
}

pub(super) fn unavailable_state(
    stage: &str,
    endpoint: Option<ApplicationEndpoint>,
) -> ApplicationDeploymentState {
    state(
        ApplicationLifecycleState::Unavailable,
        ApplicationReadinessState::NotReady,
        stage,
        endpoint,
        Vec::new(),
    )
}

pub(super) fn deployment_failure_state(
    stage: &'static str,
    failure: Option<super::errors::DeploymentFailureCode>,
) -> ApplicationDeploymentState {
    let mut state = unavailable_state(stage, None);
    state.failure = failure.map(super::errors::DeploymentFailureCode::summary);
    state
}

/// Starting state after a failed workload-group creation, carrying `failure` when the runtime
/// named a stable cause and the generic creation failure otherwise.
pub(super) fn workload_creation_failure_state(
    group: &WorkloadGroupLaunch,
    failure: Option<super::errors::DeploymentFailureCode>,
) -> ApplicationDeploymentState {
    let mut state = state(
        ApplicationLifecycleState::Starting,
        ApplicationReadinessState::NotReady,
        "starting",
        None,
        group
            .units
            .keys()
            .map(|name| ApplicationServiceObservation {
                service_name: name.0.clone(),
                lifecycle_state: ApplicationServiceLifecycleState::Pending as i32,
                readiness_state: ApplicationReadinessState::NotReady as i32,
            })
            .collect(),
    );
    state.failure = Some(match failure {
        Some(failure) => failure.summary(),
        None => ApplicationDeploymentFailure {
            code: "CreationFailed".to_owned(),
            message: "Application workload creation failed".to_owned(),
            diagnostics: Vec::new(),
        },
    });
    state
}

pub(super) fn ready_state(
    group: &ObservedWorkloadGroup,
    endpoint: ApplicationEndpoint,
) -> ApplicationDeploymentState {
    let services = group
        .units
        .keys()
        .map(|name| ApplicationServiceObservation {
            service_name: name.0.clone(),
            lifecycle_state: ApplicationServiceLifecycleState::Running as i32,
            readiness_state: ApplicationReadinessState::Ready as i32,
        })
        .collect();
    state(
        ApplicationLifecycleState::Ready,
        ApplicationReadinessState::Ready,
        "ready",
        Some(endpoint),
        services,
    )
}

pub(super) fn desired_state(
    deployment: &ApplicationDeploymentDeclaration,
) -> Result<ApplicationDeploymentDesiredState, ApplicationLifecycleError> {
    match ApplicationDeploymentDesiredState::try_from(deployment.desired_state) {
        Ok(
            state @ (ApplicationDeploymentDesiredState::Present
            | ApplicationDeploymentDesiredState::Paused
            | ApplicationDeploymentDesiredState::Teardown),
        ) => Ok(state),
        _ => Err(configuration(
            "Application deployment declaration must specify PRESENT, PAUSED, or TEARDOWN desired state",
        )),
    }
}

pub(super) fn declaration_error(error: ApplicationRepositoryError) -> ApplicationLifecycleError {
    match error {
        error @ (ApplicationRepositoryError::OutcomeUncertain { .. }
        | ApplicationRepositoryError::Poisoned) => super::outcome_uncertain_with_source(
            "Application desired state persistence outcome is uncertain",
            error,
        ),
        ApplicationRepositoryError::NotFound => ApplicationLifecycleError::NotFound,
        ApplicationRepositoryError::InvalidName(_) => ApplicationLifecycleError::InvalidName,
        ApplicationRepositoryError::AlreadyExists
        | ApplicationRepositoryError::RevisionConflict
        | ApplicationRepositoryError::InvalidTransition
        | ApplicationRepositoryError::TeardownMismatch => {
            ApplicationLifecycleError::FailedPrecondition
        }
        error => {
            super::configuration_with_source("Application desired state persistence failed", error)
        }
    }
}

pub(super) fn current_error(error: ApplicationRepositoryError) -> ApplicationLifecycleError {
    match error {
        error @ (ApplicationRepositoryError::OutcomeUncertain { .. }
        | ApplicationRepositoryError::Poisoned) => super::outcome_uncertain_with_source(
            "Application current state persistence outcome is uncertain",
            error,
        ),
        error => {
            super::configuration_with_source("Application current state persistence failed", error)
        }
    }
}

pub(super) fn partial_current_error(
    operation: &str,
    error: ApplicationRepositoryError,
) -> ApplicationLifecycleError {
    super::outcome_uncertain_with_source(
        format!(
            "{operation} accepted application desired state but could not persist current state; retry the operation"
        ),
        error,
    )
}

/// Settles a rebuilt state against the retained one. Returns `None` when the rebuilt state equals
/// the retained one apart from timestamps, so the retained record stays untouched. Otherwise
/// returns the rebuilt state stamped with a fresh `updated_at` and the retained `started_at`.
pub(super) fn settled_state(
    previous: Option<&ApplicationDeploymentState>,
    mut next: ApplicationDeploymentState,
) -> Option<ApplicationDeploymentState> {
    let Some(previous) = previous else {
        return Some(next);
    };
    let unchanged = ApplicationDeploymentState {
        started_at: previous.started_at,
        updated_at: previous.updated_at,
        ..next.clone()
    } == *previous;
    if unchanged {
        return None;
    }
    next.started_at = previous.started_at;
    next.updated_at = Some(now());
    Some(next)
}

/// Sets the start steps of a state about to be recorded, the one place they are decided. A
/// deployment that prepares or starts without a failure reports every step of a start and the one
/// its stage stands for; any other state reports none. `previous` holds the steps already
/// recorded in this start, so the current step never moves back, as when a unit restarts.
pub(super) fn with_start_steps(
    previous: Option<&ApplicationDeploymentSteps>,
    mut state: ApplicationDeploymentState,
) -> ApplicationDeploymentState {
    state.steps = start_step(&state).map(|step| {
        let current = START_STEPS
            .iter()
            .position(|candidate| *candidate == step)
            .unwrap_or_default() as u32;
        ApplicationDeploymentSteps {
            steps: START_STEPS.iter().map(|step| *step as i32).collect(),
            current: previous.map_or(current, |previous| current.max(previous.current)),
        }
    });
    state
}

/// Every step of one application start, in order. A step a start does not need, such as image
/// preparation on a runtime that launches from references alone, is passed.
const START_STEPS: [ApplicationDeploymentStep; 3] = [
    ApplicationDeploymentStep::PreparingImage,
    ApplicationDeploymentStep::Starting,
    ApplicationDeploymentStep::WaitingForEndpoint,
];

/// The start step a state stands for, `None` outside a start or once it failed. An accepted start
/// that has not prepared its image yet is on the first step.
fn start_step(state: &ApplicationDeploymentState) -> Option<ApplicationDeploymentStep> {
    if state.failure.is_some() {
        return None;
    }
    match ApplicationLifecycleState::try_from(state.lifecycle_state).ok()? {
        ApplicationLifecycleState::Preparing => Some(ApplicationDeploymentStep::PreparingImage),
        ApplicationLifecycleState::Starting => match state.stage.as_str() {
            "starting" => Some(ApplicationDeploymentStep::PreparingImage),
            GROUP_NOT_RUNNING_STAGE | ENDPOINT_NOT_ASSIGNED_STAGE => {
                Some(ApplicationDeploymentStep::Starting)
            }
            ENDPOINT_NOT_READY_STAGE => Some(ApplicationDeploymentStep::WaitingForEndpoint),
            _ => None,
        },
        _ => None,
    }
}

fn state(
    lifecycle: ApplicationLifecycleState,
    readiness: ApplicationReadinessState,
    stage: &str,
    endpoint: Option<ApplicationEndpoint>,
    services: Vec<ApplicationServiceObservation>,
) -> ApplicationDeploymentState {
    ApplicationDeploymentState {
        lifecycle_state: lifecycle as i32,
        readiness_state: readiness as i32,
        stage: stage.to_owned(),
        started_at: Some(now()),
        services,
        endpoint,
        updated_at: Some(now()),
        failure: None,
        observed_placement: None,
        observation_stale: false,
        steps: None,
    }
}

pub(super) fn now() -> Timestamp {
    let duration = real_time::wall_now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::application::v1::{
        ApplicationDeploymentFailure, ApplicationDeploymentState, ApplicationEndpoint,
        ApplicationLifecycleState, ApplicationReadinessState, ApplicationServiceLifecycleState,
        ApplicationServiceObservation,
    };

    use super::{
        booting_state, deployment_failure_state, paused_state, pausing_state, preparing_state,
        queued_state, settled_state, starting_state, stopped_state, unavailable_state,
        with_start_steps,
    };
    use crate::application::controller::readiness::{
        ENDPOINT_NOT_ASSIGNED_STAGE, ENDPOINT_NOT_READY_STAGE, GROUP_NOT_RUNNING_STAGE,
    };
    use crate::application::controller::test_support::start_steps;

    #[test]
    fn pause_states_clear_public_readiness_and_endpoint_while_preserving_history() {
        let previous = ApplicationDeploymentState {
            steps: None,
            lifecycle_state: ApplicationLifecycleState::Ready as i32,
            readiness_state: ApplicationReadinessState::Ready as i32,
            stage: "ready".to_owned(),
            started_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 11,
                nanos: 12,
            }),
            updated_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 13,
                nanos: 14,
            }),
            failure: Some(ApplicationDeploymentFailure {
                code: "runtime".to_owned(),
                message: "failed".to_owned(),
                diagnostics: vec!["detail".to_owned()],
            }),
            services: vec![ApplicationServiceObservation {
                service_name: "web".to_owned(),
                lifecycle_state: ApplicationServiceLifecycleState::Running as i32,
                readiness_state: ApplicationReadinessState::Ready as i32,
            }],
            endpoint: Some(ApplicationEndpoint {
                url: "http://127.0.0.1:8080".to_owned(),
            }),
            observed_placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "node-one".to_owned(),
                execution_target_id: "application-target-one".to_owned(),
            }),
            observation_stale: false,
        };

        let pausing = pausing_state(Some(&previous));
        assert_eq!(
            pausing.lifecycle_state,
            ApplicationLifecycleState::Pausing as i32
        );
        assert_eq!(
            pausing.readiness_state,
            ApplicationReadinessState::NotReady as i32
        );
        assert_eq!(pausing.stage, "pausing");
        assert_eq!(pausing.started_at, previous.started_at);
        assert_eq!(pausing.failure, previous.failure);
        assert!(pausing.services.is_empty());
        assert_eq!(pausing.endpoint, None);
        assert_eq!(pausing.observed_placement, previous.observed_placement);

        let paused = paused_state(Some(&pausing));
        assert_eq!(
            paused.lifecycle_state,
            ApplicationLifecycleState::Paused as i32
        );
        assert_eq!(paused.stage, "paused");
        assert_eq!(paused.started_at, previous.started_at);
        assert_eq!(paused.failure, previous.failure);
        assert!(paused.services.is_empty());
        assert_eq!(paused.endpoint, None);
        assert_eq!(paused.observed_placement, previous.observed_placement);
    }

    #[test]
    fn resume_acceptance_uses_starting_without_republishing_endpoint() {
        let previous = pausing_state(None);
        let starting = starting_state(Some(&previous));

        assert_eq!(
            starting.lifecycle_state,
            ApplicationLifecycleState::Starting as i32
        );
        assert_eq!(
            starting.readiness_state,
            ApplicationReadinessState::NotReady as i32
        );
        assert_eq!(starting.stage, "starting");
        assert_eq!(starting.endpoint, None);
    }

    #[test]
    fn each_start_state_reports_every_step_and_the_one_its_stage_stands_for() {
        let booting = |stage| booting_state(unavailable_state(stage, None));
        let cases = [
            ("accepted start", starting_state(None), 0),
            ("preparing image", preparing_state(None), 0),
            ("group not running", booting(GROUP_NOT_RUNNING_STAGE), 1),
            (
                "endpoint not assigned",
                booting(ENDPOINT_NOT_ASSIGNED_STAGE),
                1,
            ),
            ("endpoint not ready", booting(ENDPOINT_NOT_READY_STAGE), 2),
        ];
        for (case, state, current) in cases {
            assert_eq!(
                with_start_steps(None, state).steps,
                Some(start_steps(current)),
                "{case}"
            );
        }
    }

    #[test]
    fn the_current_step_never_moves_back_within_a_start() {
        let restarted = booting_state(unavailable_state(GROUP_NOT_RUNNING_STAGE, None));
        let recorded = start_steps(2);

        assert_eq!(
            with_start_steps(Some(&recorded), restarted.clone()).steps,
            Some(start_steps(2))
        );
        assert_eq!(
            with_start_steps(Some(&start_steps(0)), restarted).steps,
            Some(start_steps(1))
        );
    }

    #[test]
    fn states_outside_a_start_or_with_a_failure_report_no_steps() {
        let mut failed_creation = starting_state(None);
        failed_creation.failure = Some(ApplicationDeploymentFailure {
            code: "CreationFailed".to_owned(),
            message: "Application workload creation failed".to_owned(),
            diagnostics: Vec::new(),
        });
        let mut reconciling = starting_state(None);
        reconciling.stage = "reconciling".to_owned();
        let mut ready = unavailable_state("ready", None);
        ready.lifecycle_state = ApplicationLifecycleState::Ready as i32;
        let cases = [
            ("queued", queued_state()),
            ("ready", ready),
            ("pausing", pausing_state(None)),
            ("paused", paused_state(None)),
            ("stopped", stopped_state()),
            (
                "unavailable",
                unavailable_state(ENDPOINT_NOT_READY_STAGE, None),
            ),
            (
                "failed",
                deployment_failure_state("artifact-preparation", None),
            ),
            ("failed creation", failed_creation),
            ("reconciling", reconciling),
        ];
        for (case, state) in cases {
            assert_eq!(
                with_start_steps(Some(&start_steps(2)), state).steps,
                None,
                "{case}"
            );
        }
    }

    #[test]
    fn preparing_starts_afresh_from_an_earlier_start_and_keeps_its_history() {
        let mut previous = paused_state(None);
        previous.started_at = Some(timestamp(11));
        previous.failure = Some(ApplicationDeploymentFailure {
            code: "runtime".to_owned(),
            message: "failed".to_owned(),
            diagnostics: Vec::new(),
        });
        previous.endpoint = Some(ApplicationEndpoint {
            url: "http://127.0.0.1:8080".to_owned(),
        });

        let preparing = preparing_state(Some(&previous));

        assert_eq!(
            preparing.lifecycle_state,
            ApplicationLifecycleState::Preparing as i32
        );
        assert_eq!(
            preparing.readiness_state,
            ApplicationReadinessState::NotReady as i32
        );
        assert_eq!(preparing.stage, "preparing");
        assert_eq!(preparing.started_at, Some(timestamp(11)));
        assert_eq!(preparing.failure, None);
        assert_eq!(preparing.endpoint, None);
    }

    fn timestamp(seconds: i64) -> dotcore_proto::google::protobuf::Timestamp {
        dotcore_proto::google::protobuf::Timestamp { seconds, nanos: 0 }
    }

    #[test]
    fn settled_state_skips_an_equal_rebuild_and_stamps_a_real_transition() {
        let mut previous = unavailable_state("Application primary endpoint is not ready", None);
        previous.started_at = Some(timestamp(11));
        previous.updated_at = Some(timestamp(13));

        assert_eq!(
            settled_state(
                Some(&previous),
                unavailable_state("Application primary endpoint is not ready", None),
            ),
            None
        );

        let settled = settled_state(
            Some(&previous),
            unavailable_state("Application group is not running", None),
        )
        .expect("transition is written");
        assert_eq!(settled.stage, "Application group is not running");
        assert_eq!(settled.started_at, Some(timestamp(11)));
        assert!(settled.updated_at.expect("updated_at").seconds > 13);
    }
}
