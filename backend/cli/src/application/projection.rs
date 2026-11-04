//! The words shown for an application deployment's name and state, matching the web console,
//! for example "Pausing" as soon as a pause is requested.
//!
//! The manager reports the observed lifecycle and the requested desired state separately; the
//! state word combines them the way the console does, so a requested pause or teardown shows
//! before the workloads catch up. The words are copied from the console and can drift from it.

use dotcore_proto::dotcore::application::v1::{
    ApplicationDeployment, ApplicationDeploymentDesiredState, ApplicationLifecycleState,
    ApplicationReadinessState, ApplicationServiceLifecycleState,
};

use crate::management::Addressable;

/// The state word of the deployment, for example `Pausing`. A requested teardown reads
/// `Tearing down`, a requested pause reads `Pausing` until the workloads are paused or
/// unavailable, and a deployment reported up but not serving reads `Not ready`.
pub(crate) fn state_label(deployment: &ApplicationDeployment) -> &'static str {
    if deployment.desired_state() == ApplicationDeploymentDesiredState::Teardown {
        return "Tearing down";
    }

    let lifecycle = effective_lifecycle(deployment);
    let up = matches!(
        lifecycle,
        ApplicationLifecycleState::Running | ApplicationLifecycleState::Ready
    );
    if up
        && !observation_stale(deployment)
        && readiness(deployment) == ApplicationReadinessState::NotReady
    {
        return readiness_label(ApplicationReadinessState::NotReady);
    }

    lifecycle_label(lifecycle)
}

/// Whether a requested teardown or pause replaces the observed lifecycle, so text the workloads
/// reported about that lifecycle no longer describes the shown state.
pub(crate) fn state_overridden(deployment: &ApplicationDeployment) -> bool {
    let observed = deployment
        .state
        .as_ref()
        .map_or(ApplicationLifecycleState::Unspecified, |state| {
            state.lifecycle_state()
        });

    deployment.desired_state() == ApplicationDeploymentDesiredState::Teardown
        || effective_lifecycle(deployment) != observed
}

/// The readiness the operator sees: none while the deployment is asked to pause.
pub(crate) fn readiness(deployment: &ApplicationDeployment) -> ApplicationReadinessState {
    if deployment.desired_state() == ApplicationDeploymentDesiredState::Paused {
        return ApplicationReadinessState::Unspecified;
    }

    deployment
        .state
        .as_ref()
        .map_or(ApplicationReadinessState::Unspecified, |state| {
            state.readiness_state()
        })
}

/// The word for one readiness; unspecified and unknown values read `Unknown`.
pub(crate) fn readiness_label(readiness: ApplicationReadinessState) -> &'static str {
    match readiness {
        ApplicationReadinessState::Ready => "Ready",
        ApplicationReadinessState::NotReady => "Not ready",
        ApplicationReadinessState::Unspecified => "Unknown",
    }
}

/// The word for the desired state the manager drives the deployment toward.
pub(crate) fn desired_state_label(desired: ApplicationDeploymentDesiredState) -> &'static str {
    match desired {
        ApplicationDeploymentDesiredState::Present => "Present",
        ApplicationDeploymentDesiredState::Paused => "Paused",
        ApplicationDeploymentDesiredState::Teardown => "Teardown",
        ApplicationDeploymentDesiredState::Unspecified => "Unspecified",
    }
}

/// The word for one service lifecycle; unspecified and unknown values read `Unknown`.
pub(crate) fn service_label(state: ApplicationServiceLifecycleState) -> &'static str {
    match state {
        ApplicationServiceLifecycleState::Pending => "Pending",
        ApplicationServiceLifecycleState::Starting => "Starting",
        ApplicationServiceLifecycleState::Running => "Running",
        ApplicationServiceLifecycleState::Stopped => "Stopped",
        ApplicationServiceLifecycleState::Failed => "Failed",
        ApplicationServiceLifecycleState::Unavailable => "Unavailable",
        ApplicationServiceLifecycleState::Unspecified => "Unknown",
    }
}

/// Whether the shown state is the last report of a node the manager can no longer confirm.
pub(crate) fn observation_stale(deployment: &ApplicationDeployment) -> bool {
    deployment
        .state
        .as_ref()
        .is_some_and(|state| state.observation_stale)
}

/// The endpoint URL to open, none while the deployment is asked to pause.
pub(crate) fn endpoint(deployment: &ApplicationDeployment) -> &str {
    if deployment.desired_state() == ApplicationDeploymentDesiredState::Paused {
        return "";
    }

    deployment
        .state
        .as_ref()
        .and_then(|state| state.endpoint.as_ref())
        .map_or("", |endpoint| endpoint.url.as_str())
}

/// The deployment's name unless it is empty or a generated default (its id, or the application id
/// with `-1`), else the catalog application's display name, else `Application`.
pub(crate) fn presentation_name(deployment: &ApplicationDeployment) -> &str {
    let name = deployment.application_deployment_name.as_str();
    let application_id = deployment
        .snapshot
        .as_ref()
        .map_or("", |snapshot| snapshot.application_id.as_str());
    if !name.is_empty()
        && name != deployment.application_deployment_id
        && name != format!("{application_id}-1")
    {
        return name;
    }

    application_display_name(deployment)
}

/// The catalog application's display name, else `Application`.
pub(crate) fn application_display_name(deployment: &ApplicationDeployment) -> &str {
    deployment
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.display_name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Application")
}

/// The lifecycle the operator sees: a requested pause reads as pausing until the workloads are
/// pausing, paused or unavailable.
fn effective_lifecycle(deployment: &ApplicationDeployment) -> ApplicationLifecycleState {
    let lifecycle = deployment
        .state
        .as_ref()
        .map_or(ApplicationLifecycleState::Unspecified, |state| {
            state.lifecycle_state()
        });
    let paused = matches!(
        lifecycle,
        ApplicationLifecycleState::Pausing
            | ApplicationLifecycleState::Paused
            | ApplicationLifecycleState::Unavailable
    );
    if deployment.desired_state() == ApplicationDeploymentDesiredState::Paused && !paused {
        return ApplicationLifecycleState::Pausing;
    }

    lifecycle
}

/// The word for one lifecycle; unspecified and unknown values read `Unknown`.
fn lifecycle_label(state: ApplicationLifecycleState) -> &'static str {
    match state {
        ApplicationLifecycleState::Queued => "Queued",
        ApplicationLifecycleState::Preparing => "Preparing",
        ApplicationLifecycleState::Starting => "Starting",
        ApplicationLifecycleState::Running => "Running",
        ApplicationLifecycleState::Ready => "Ready",
        ApplicationLifecycleState::Stopping => "Stopping",
        ApplicationLifecycleState::Pausing => "Pausing",
        ApplicationLifecycleState::Paused => "Paused",
        ApplicationLifecycleState::Stopped => "Stopped",
        ApplicationLifecycleState::Failed => "Failed",
        ApplicationLifecycleState::Unavailable => "Unavailable",
        ApplicationLifecycleState::Unspecified => "Unknown",
    }
}

impl Addressable for ApplicationDeployment {
    fn id(&self) -> &str {
        &self.application_deployment_id
    }

    fn name(&self) -> &str {
        presentation_name(self)
    }

    fn stored_name(&self) -> &str {
        &self.application_deployment_name
    }
}

#[cfg(test)]
mod tests {
    use ApplicationDeploymentDesiredState::{Paused, Present, Teardown};
    use ApplicationLifecycleState::{Pausing, Ready, Running, Unavailable};
    use dotcore_proto::dotcore::application::v1::{
        ApplicationDeploymentState, ApplicationEndpoint, ResolvedApplicationDeploymentSnapshot,
    };

    use super::*;

    const EVERY_LIFECYCLE: [(ApplicationLifecycleState, &str); 12] = [
        (ApplicationLifecycleState::Unspecified, "Unknown"),
        (ApplicationLifecycleState::Queued, "Queued"),
        (ApplicationLifecycleState::Preparing, "Preparing"),
        (ApplicationLifecycleState::Starting, "Starting"),
        (ApplicationLifecycleState::Running, "Running"),
        (ApplicationLifecycleState::Ready, "Ready"),
        (ApplicationLifecycleState::Stopping, "Stopping"),
        (ApplicationLifecycleState::Stopped, "Stopped"),
        (ApplicationLifecycleState::Failed, "Failed"),
        (ApplicationLifecycleState::Unavailable, "Unavailable"),
        (ApplicationLifecycleState::Pausing, "Pausing"),
        (ApplicationLifecycleState::Paused, "Paused"),
    ];

    fn deployment(
        lifecycle: i32,
        readiness: ApplicationReadinessState,
        desired: ApplicationDeploymentDesiredState,
    ) -> ApplicationDeployment {
        ApplicationDeployment {
            state: Some(ApplicationDeploymentState {
                lifecycle_state: lifecycle,
                readiness_state: readiness as i32,
                ..Default::default()
            }),
            desired_state: desired as i32,
            ..Default::default()
        }
    }

    #[test]
    fn present_deployments_show_their_lifecycle() {
        for (lifecycle, label) in EVERY_LIFECYCLE {
            assert_eq!(
                state_label(&deployment(
                    lifecycle as i32,
                    ApplicationReadinessState::Unspecified,
                    Present
                )),
                label,
                "{lifecycle:?}"
            );
        }
        assert_eq!(
            state_label(&deployment(
                99,
                ApplicationReadinessState::Unspecified,
                Present
            )),
            "Unknown",
            "a lifecycle from a newer manager"
        );
        assert_eq!(state_label(&ApplicationDeployment::default()), "Unknown");
    }

    #[test]
    fn a_requested_teardown_shows_tearing_down_whatever_the_lifecycle() {
        for (lifecycle, _) in EVERY_LIFECYCLE {
            assert_eq!(
                state_label(&deployment(
                    lifecycle as i32,
                    ApplicationReadinessState::Ready,
                    Teardown
                )),
                "Tearing down",
                "{lifecycle:?}"
            );
        }
    }

    #[test]
    fn a_requested_pause_shows_pausing_until_pausing_paused_or_unavailable() {
        for (lifecycle, label) in EVERY_LIFECYCLE {
            let expected = match lifecycle {
                ApplicationLifecycleState::Pausing
                | ApplicationLifecycleState::Paused
                | ApplicationLifecycleState::Unavailable => label,
                _ => "Pausing",
            };
            assert_eq!(
                state_label(&deployment(
                    lifecycle as i32,
                    ApplicationReadinessState::Ready,
                    Paused
                )),
                expected,
                "{lifecycle:?}"
            );
        }
    }

    #[test]
    fn an_up_deployment_that_is_not_serving_reads_not_ready() {
        for lifecycle in [Running, Ready] {
            assert_eq!(
                state_label(&deployment(
                    lifecycle as i32,
                    ApplicationReadinessState::NotReady,
                    Present
                )),
                "Not ready",
                "{lifecycle:?}"
            );
        }
        assert_eq!(
            state_label(&deployment(
                Pausing as i32,
                ApplicationReadinessState::NotReady,
                Present
            )),
            "Pausing"
        );
        assert_eq!(
            state_label(&deployment(
                Unavailable as i32,
                ApplicationReadinessState::NotReady,
                Present
            )),
            "Unavailable"
        );
    }

    #[test]
    fn a_stale_observation_keeps_the_lifecycle_word() {
        let mut stale = deployment(Running as i32, ApplicationReadinessState::NotReady, Present);
        stale.state.as_mut().expect("state").observation_stale = true;

        assert_eq!(state_label(&stale), "Running");
        assert!(observation_stale(&stale));
    }

    #[test]
    fn a_requested_pause_hides_readiness_and_endpoint() {
        let mut running = deployment(Ready as i32, ApplicationReadinessState::Ready, Present);
        running.state.as_mut().expect("state").endpoint = Some(ApplicationEndpoint {
            url: "http://gb10-host:8080".to_owned(),
        });

        assert_eq!(readiness(&running), ApplicationReadinessState::Ready);
        assert_eq!(endpoint(&running), "http://gb10-host:8080");

        running.desired_state = Paused as i32;

        assert_eq!(readiness(&running), ApplicationReadinessState::Unspecified);
        assert_eq!(endpoint(&running), "");
    }

    #[test]
    fn labels_cover_readiness_desired_states_and_services() {
        assert_eq!(
            [
                ApplicationReadinessState::Unspecified,
                ApplicationReadinessState::NotReady,
                ApplicationReadinessState::Ready,
            ]
            .map(readiness_label),
            ["Unknown", "Not ready", "Ready"]
        );
        assert_eq!(
            [
                ApplicationDeploymentDesiredState::Unspecified,
                Present,
                Paused,
                Teardown,
            ]
            .map(desired_state_label),
            ["Unspecified", "Present", "Paused", "Teardown"]
        );
        assert_eq!(
            [
                ApplicationServiceLifecycleState::Unspecified,
                ApplicationServiceLifecycleState::Pending,
                ApplicationServiceLifecycleState::Starting,
                ApplicationServiceLifecycleState::Running,
                ApplicationServiceLifecycleState::Stopped,
                ApplicationServiceLifecycleState::Failed,
                ApplicationServiceLifecycleState::Unavailable,
            ]
            .map(service_label),
            [
                "Unknown",
                "Pending",
                "Starting",
                "Running",
                "Stopped",
                "Failed",
                "Unavailable"
            ]
        );
    }

    #[test]
    fn the_name_falls_back_from_generated_defaults_to_the_application() {
        let named = |name: &str| ApplicationDeployment {
            application_deployment_id: "app-dep-1".to_owned(),
            application_deployment_name: name.to_owned(),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                application_id: "open-webui".to_owned(),
                display_name: "Open WebUI".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(named("Chat").name(), "Chat");
        assert_eq!(named("").name(), "Open WebUI");
        assert_eq!(named("app-dep-1").name(), "Open WebUI");
        assert_eq!(named("open-webui-1").name(), "Open WebUI");
        assert_eq!(named("Chat").id(), "app-dep-1");
        assert_eq!(ApplicationDeployment::default().name(), "Application");
    }
}
