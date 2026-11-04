//! Apps-controller state values and repository error mappings.

use dotcore_runtime::{ObservedWorkloadGroup, WorkloadGroupLaunch};

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
    }
}

pub(super) fn now() -> Timestamp {
    let duration = SystemTime::now()
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

    use super::{paused_state, pausing_state, settled_state, starting_state, unavailable_state};

    #[test]
    fn pause_states_clear_public_readiness_and_endpoint_while_preserving_history() {
        let previous = ApplicationDeploymentState {
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
