//! The words shown for a model deployment's name and state, matching the web console, for
//! example "Tearing down" as soon as a teardown is requested.
//!
//! The manager reports the observed status and the requested desired state separately; the state
//! word combines them the way the console does, so a requested pause or teardown shows before the
//! workload catches up. The words are copied from the console and can drift from it.

use dotcore_proto::dotcore::model::v1::{
    ModelDeployment, ModelDeploymentDesiredState, ModelDeploymentStatus, model_deployment_snapshot,
};

use crate::management::Addressable;

/// The status the operator sees: a requested teardown reads as unloading, and a requested pause
/// reads as pausing until the workload is paused or unavailable.
pub(crate) fn effective_status(deployment: &ModelDeployment) -> ModelDeploymentStatus {
    let desired = deployment.desired_state();
    if desired == ModelDeploymentDesiredState::Teardown {
        return ModelDeploymentStatus::Unloading;
    }

    let status = deployment
        .state
        .as_ref()
        .map_or(ModelDeploymentStatus::Unspecified, |state| state.status());
    let paused = matches!(
        status,
        ModelDeploymentStatus::Pausing
            | ModelDeploymentStatus::Paused
            | ModelDeploymentStatus::Unavailable
    );
    if desired == ModelDeploymentDesiredState::Paused && !paused {
        return ModelDeploymentStatus::Pausing;
    }

    status
}

/// Whether a requested teardown or pause replaces the observed status, so text the workload
/// reported about that status no longer describes the shown state.
pub(crate) fn state_overridden(deployment: &ModelDeployment) -> bool {
    let observed = deployment
        .state
        .as_ref()
        .map_or(ModelDeploymentStatus::Unspecified, |state| state.status());

    effective_status(deployment) != observed
}

/// The state word of the deployment, for example `Pausing`.
pub(crate) fn state_label(deployment: &ModelDeployment) -> &'static str {
    status_label(effective_status(deployment))
}

/// The word for one status; unspecified and unknown values read `Unknown`.
pub(crate) fn status_label(status: ModelDeploymentStatus) -> &'static str {
    match status {
        ModelDeploymentStatus::Queued => "Queued",
        ModelDeploymentStatus::Preparing => "Preparing",
        ModelDeploymentStatus::Downloading => "Downloading",
        ModelDeploymentStatus::Loading => "Loading",
        ModelDeploymentStatus::WarmingUp => "Warming up",
        ModelDeploymentStatus::Ready => "Ready",
        ModelDeploymentStatus::Unloading => "Tearing down",
        ModelDeploymentStatus::Pausing => "Pausing",
        ModelDeploymentStatus::Paused => "Paused",
        ModelDeploymentStatus::Failed => "Failed",
        ModelDeploymentStatus::Unavailable => "Unavailable",
        ModelDeploymentStatus::Unspecified => "Unknown",
    }
}

/// The word for the desired state the manager drives the deployment toward.
pub(crate) fn desired_state_label(desired: ModelDeploymentDesiredState) -> &'static str {
    match desired {
        ModelDeploymentDesiredState::Present => "Present",
        ModelDeploymentDesiredState::Paused => "Paused",
        ModelDeploymentDesiredState::Teardown => "Teardown",
        ModelDeploymentDesiredState::Unspecified => "Unspecified",
    }
}

/// The deployment's name, else its catalog model's display name, else `Upstream model`.
pub(crate) fn presentation_name(deployment: &ModelDeployment) -> &str {
    if !deployment.model_deployment_name.is_empty() {
        return &deployment.model_deployment_name;
    }
    model_display_name(deployment)
}

/// The catalog model's display name for a local deployment, else `Upstream model`.
pub(crate) fn model_display_name(deployment: &ModelDeployment) -> &str {
    match deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    {
        Some(model_deployment_snapshot::Source::Local(local)) => &local.model_display_name,
        _ => "Upstream model",
    }
}

impl Addressable for ModelDeployment {
    fn id(&self) -> &str {
        &self.model_deployment_id
    }

    fn name(&self) -> &str {
        presentation_name(self)
    }

    fn stored_name(&self) -> &str {
        &self.model_deployment_name
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::model::v1::{
        ModelDeploymentSnapshot, ModelDeploymentState, ResolvedLocalModelDeploymentSnapshot,
        UpstreamModelDeploymentSnapshot,
    };

    use super::*;

    const EVERY_STATUS: [(ModelDeploymentStatus, &str); 12] = [
        (ModelDeploymentStatus::Unspecified, "Unknown"),
        (ModelDeploymentStatus::Queued, "Queued"),
        (ModelDeploymentStatus::Preparing, "Preparing"),
        (ModelDeploymentStatus::Downloading, "Downloading"),
        (ModelDeploymentStatus::Loading, "Loading"),
        (ModelDeploymentStatus::WarmingUp, "Warming up"),
        (ModelDeploymentStatus::Ready, "Ready"),
        (ModelDeploymentStatus::Unloading, "Tearing down"),
        (ModelDeploymentStatus::Pausing, "Pausing"),
        (ModelDeploymentStatus::Paused, "Paused"),
        (ModelDeploymentStatus::Failed, "Failed"),
        (ModelDeploymentStatus::Unavailable, "Unavailable"),
    ];

    fn deployment(status: i32, desired: ModelDeploymentDesiredState) -> ModelDeployment {
        ModelDeployment {
            state: Some(ModelDeploymentState {
                status,
                ..Default::default()
            }),
            desired_state: desired as i32,
            ..Default::default()
        }
    }

    #[test]
    fn present_deployments_show_their_observed_status() {
        for (status, label) in EVERY_STATUS {
            assert_eq!(
                state_label(&deployment(
                    status as i32,
                    ModelDeploymentDesiredState::Present
                )),
                label,
                "{status:?}"
            );
        }
        assert_eq!(
            state_label(&deployment(99, ModelDeploymentDesiredState::Present)),
            "Unknown",
            "a status from a newer manager"
        );
        assert_eq!(
            state_label(&ModelDeployment::default()),
            "Unknown",
            "no state reported yet"
        );
    }

    #[test]
    fn a_requested_teardown_shows_tearing_down_whatever_the_status() {
        for (status, _) in EVERY_STATUS {
            assert_eq!(
                state_label(&deployment(
                    status as i32,
                    ModelDeploymentDesiredState::Teardown
                )),
                "Tearing down",
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_requested_pause_shows_pausing_until_paused_or_unavailable() {
        for (status, _) in EVERY_STATUS {
            let expected = match status {
                ModelDeploymentStatus::Paused => "Paused",
                ModelDeploymentStatus::Unavailable => "Unavailable",
                _ => "Pausing",
            };
            assert_eq!(
                state_label(&deployment(
                    status as i32,
                    ModelDeploymentDesiredState::Paused
                )),
                expected,
                "{status:?}"
            );
        }
    }

    #[test]
    fn desired_states_have_console_words() {
        assert_eq!(
            [
                ModelDeploymentDesiredState::Unspecified,
                ModelDeploymentDesiredState::Present,
                ModelDeploymentDesiredState::Paused,
                ModelDeploymentDesiredState::Teardown,
            ]
            .map(desired_state_label),
            ["Unspecified", "Present", "Paused", "Teardown"]
        );
    }

    #[test]
    fn the_name_falls_back_to_the_model_then_to_upstream() {
        let local = |name: &str| ModelDeployment {
            model_deployment_id: "model-1".to_owned(),
            model_deployment_name: name.to_owned(),
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(model_deployment_snapshot::Source::Local(
                    ResolvedLocalModelDeploymentSnapshot {
                        model_display_name: "Gemma 4 E4B".to_owned(),
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        };
        let upstream = ModelDeployment {
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(model_deployment_snapshot::Source::Upstream(
                    UpstreamModelDeploymentSnapshot::default(),
                )),
            }),
            ..Default::default()
        };

        assert_eq!(local("Chat").name(), "Chat");
        assert_eq!(local("").name(), "Gemma 4 E4B");
        assert_eq!(local("").id(), "model-1");
        assert_eq!(upstream.name(), "Upstream model");
    }
}
