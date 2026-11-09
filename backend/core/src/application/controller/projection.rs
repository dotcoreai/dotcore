//! Apps-controller projection of declarations and observed state.

use dotcore_proto::dotcore::application::v1::{
    ApplicationDeployment, ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
    ApplicationDeploymentState, ApplicationLifecycleState, ApplicationReadinessState,
    ApplicationSelectedInferenceRoute,
};

use super::{ApplicationLifecycleError, desired_state};

pub(super) fn project(
    desired: &ApplicationDeploymentDeclaration,
    state: ApplicationDeploymentState,
    selected_inference_route: Option<ApplicationSelectedInferenceRoute>,
) -> Result<ApplicationDeployment, ApplicationLifecycleError> {
    let desired_state = desired_state(desired)?;
    Ok(ApplicationDeployment {
        application_deployment_id: desired.application_deployment_id.clone(),
        application_deployment_name: desired.application_deployment_name.clone(),
        placement: desired.placement.clone(),
        snapshot: desired.snapshot.clone(),
        desired_state: desired_state as i32,
        state: Some(sanitize_state(desired_state, state)),
        storage_attachments: desired.storage_attachments.clone(),
        selected_inference_route,
        node_resolved_launch_identities: desired.node_resolved_launch_identities.clone(),
        service_accelerator_memory: Vec::new(),
    })
}

fn sanitize_state(
    desired_state: ApplicationDeploymentDesiredState,
    mut state: ApplicationDeploymentState,
) -> ApplicationDeploymentState {
    match desired_state {
        ApplicationDeploymentDesiredState::Paused => {
            if !matches!(
                ApplicationLifecycleState::try_from(state.lifecycle_state),
                Ok(ApplicationLifecycleState::Pausing
                    | ApplicationLifecycleState::Paused
                    | ApplicationLifecycleState::Unavailable)
            ) {
                state.lifecycle_state = ApplicationLifecycleState::Pausing as i32;
                state.stage = "pausing".to_owned();
                state.steps = None;
            }
            suppress_endpoint_projection(&mut state);
        }
        ApplicationDeploymentDesiredState::Teardown => {
            if !matches!(
                ApplicationLifecycleState::try_from(state.lifecycle_state),
                Ok(ApplicationLifecycleState::Stopping
                    | ApplicationLifecycleState::Stopped
                    | ApplicationLifecycleState::Unavailable)
            ) {
                state.lifecycle_state = ApplicationLifecycleState::Stopping as i32;
                state.stage = "stopping".to_owned();
                state.steps = None;
            }
            suppress_endpoint_projection(&mut state);
        }
        ApplicationDeploymentDesiredState::Present => {
            if matches!(
                ApplicationLifecycleState::try_from(state.lifecycle_state),
                Ok(ApplicationLifecycleState::Pausing | ApplicationLifecycleState::Paused)
            ) {
                state.lifecycle_state = ApplicationLifecycleState::Starting as i32;
                state.stage = "starting".to_owned();
                suppress_endpoint_projection(&mut state);
            }
        }
        ApplicationDeploymentDesiredState::Unspecified => {}
    }
    state
}

fn suppress_endpoint_projection(state: &mut ApplicationDeploymentState) {
    state.readiness_state = ApplicationReadinessState::NotReady as i32;
    state.services.clear();
    state.endpoint = None;
}
