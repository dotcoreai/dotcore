//! Translates between the public application status text a node's realizer records, such as
//! the stage `Application primary endpoint is not ready`, and the typed stages and failure codes
//! the node reports to the manager.
//!
//! Nodes never send free text across the node boundary: they report one typed stage and one
//! failure code, and the manager maps them back to the same text and failure summary the
//! realizer recorded, so the application service reads them as the node recorded them.

use dotcore_proto::dotcore::application::{
    private::v1::{ApplicationStatusFailureCode, ApplicationStatusStage},
    v1::{ApplicationDeploymentFailure, ApplicationDeploymentState},
};

use super::{
    errors::DeploymentFailureCode,
    readiness::{ENDPOINT_NOT_ASSIGNED_STAGE, ENDPOINT_NOT_READY_STAGE, GROUP_NOT_RUNNING_STAGE},
};

/// Failure code recorded when runtime workload group creation fails.
const CREATION_FAILED: &str = "CreationFailed";

/// Every typed stage with the exact text the realizer records for it.
const STAGE_TEXT: [(ApplicationStatusStage, &str); 33] = [
    (ApplicationStatusStage::Queued, "queued"),
    (ApplicationStatusStage::Preparing, "preparing"),
    (ApplicationStatusStage::Starting, "starting"),
    (ApplicationStatusStage::Ready, "ready"),
    (ApplicationStatusStage::Pausing, "pausing"),
    (ApplicationStatusStage::Paused, "paused"),
    (ApplicationStatusStage::Stopping, "stopping"),
    (ApplicationStatusStage::Stopped, "stopped"),
    (
        ApplicationStatusStage::GroupInconclusive,
        "Runtime returned an inconclusive application group",
    ),
    (
        ApplicationStatusStage::GroupNotRunning,
        GROUP_NOT_RUNNING_STAGE,
    ),
    (
        ApplicationStatusStage::EndpointNotAssigned,
        ENDPOINT_NOT_ASSIGNED_STAGE,
    ),
    (
        ApplicationStatusStage::EndpointNotReady,
        ENDPOINT_NOT_READY_STAGE,
    ),
    (
        ApplicationStatusStage::GroupNotSafeToRemove,
        "Application runtime group is not safe to remove",
    ),
    (
        ApplicationStatusStage::GroupStale,
        "Application runtime group is stale or inconclusive",
    ),
    (
        ApplicationStatusStage::GroupUnavailableWhilePaused,
        "Application runtime group is unavailable while paused",
    ),
    (
        ApplicationStatusStage::GroupRemainsAfterRemoval,
        "Application runtime group remains after removal",
    ),
    (
        ApplicationStatusStage::GroupRemovalInconclusive,
        "Application runtime group removal is inconclusive",
    ),
    (
        ApplicationStatusStage::TargetCompatibilityFailed,
        "target-compatibility",
    ),
    (
        ApplicationStatusStage::LaunchPreparationFailed,
        "launch-preparation",
    ),
    (
        ApplicationStatusStage::ArtifactPreparationFailed,
        "artifact-preparation",
    ),
    (
        ApplicationStatusStage::ReadinessOperationFailed,
        "readiness-operation",
    ),
    (
        ApplicationStatusStage::RuntimeInspectionFailed,
        "runtime-inspection",
    ),
    (
        ApplicationStatusStage::RuntimeOwnershipFailed,
        "runtime-ownership",
    ),
    (ApplicationStatusStage::RuntimeStartFailed, "runtime-start"),
    (ApplicationStatusStage::RuntimeStopFailed, "runtime-stop"),
    (
        ApplicationStatusStage::RuntimeRemovalFailed,
        "runtime-removal",
    ),
    (
        ApplicationStatusStage::RuntimeRemovalConfirmationFailed,
        "runtime-removal-confirmation",
    ),
    (
        ApplicationStatusStage::RuntimeCleanupFailed,
        "runtime-cleanup",
    ),
    (
        ApplicationStatusStage::RuntimeStaleRemovalFailed,
        "runtime-stale-removal",
    ),
    (
        ApplicationStatusStage::RuntimeReplacementStopFailed,
        "runtime-replacement-stop",
    ),
    (
        ApplicationStatusStage::RuntimeReplacementRemovalFailed,
        "runtime-replacement-removal",
    ),
    (
        ApplicationStatusStage::RuntimeReplacementCreationFailed,
        "runtime-replacement-creation",
    ),
    (
        ApplicationStatusStage::RuntimeRecoveryReplacementRemovalFailed,
        "runtime-recovery-replacement-removal",
    ),
];

/// Maps the realizer's recorded stage text to its typed stage; unknown text is unspecified.
pub(super) fn status_stage(stage: &str) -> ApplicationStatusStage {
    STAGE_TEXT
        .iter()
        .find(|(_, text)| *text == stage)
        .map_or(ApplicationStatusStage::Unspecified, |(stage, _)| *stage)
}

/// Maps a typed stage back to the text the realizer records, `None` for an unspecified stage.
pub(super) fn stage_text(stage: ApplicationStatusStage) -> Option<&'static str> {
    STAGE_TEXT
        .iter()
        .find(|(typed, _)| *typed == stage)
        .map(|(_, text)| *text)
}

/// Maps the recorded failure summary to its stable failure code.
pub(super) fn status_failure_code(
    state: &ApplicationDeploymentState,
) -> ApplicationStatusFailureCode {
    let code = state.failure.as_ref().map(|failure| failure.code.as_str());
    if code == Some(DeploymentFailureCode::RecoveryRequired.code()) {
        ApplicationStatusFailureCode::RecoveryRequired
    } else if code == Some(DeploymentFailureCode::LaunchIdentityUnresolved.code()) {
        ApplicationStatusFailureCode::LaunchIdentityUnresolved
    } else if code == Some(DeploymentFailureCode::ImageMissing.code()) {
        ApplicationStatusFailureCode::ImageMissing
    } else if code == Some(CREATION_FAILED) {
        ApplicationStatusFailureCode::CreationFailed
    } else if code == Some(DeploymentFailureCode::ArtifactPreparationFailed.code()) {
        ApplicationStatusFailureCode::ArtifactPreparationFailed
    } else if code == Some(DeploymentFailureCode::PublishedPortInUse.code()) {
        ApplicationStatusFailureCode::PublishedPortInUse
    } else {
        ApplicationStatusFailureCode::Unspecified
    }
}

/// Maps a reported failure code back to the failure summary the hosting node's realizer recorded.
/// Failures found only while checking a node assignment, such as an assignment the hosting node
/// cannot realize, get their own stable summary.
pub(super) fn public_failure(
    code: ApplicationStatusFailureCode,
) -> Option<ApplicationDeploymentFailure> {
    let (code, message) = match code {
        ApplicationStatusFailureCode::Unspecified => return None,
        ApplicationStatusFailureCode::RecoveryRequired => {
            return Some(DeploymentFailureCode::RecoveryRequired.summary());
        }
        ApplicationStatusFailureCode::LaunchIdentityUnresolved => {
            return Some(DeploymentFailureCode::LaunchIdentityUnresolved.summary());
        }
        ApplicationStatusFailureCode::ImageMissing => {
            return Some(DeploymentFailureCode::ImageMissing.summary());
        }
        ApplicationStatusFailureCode::ArtifactPreparationFailed => {
            return Some(DeploymentFailureCode::ArtifactPreparationFailed.summary());
        }
        ApplicationStatusFailureCode::PublishedPortInUse => {
            return Some(DeploymentFailureCode::PublishedPortInUse.summary());
        }
        ApplicationStatusFailureCode::CreationFailed => {
            (CREATION_FAILED, "Application workload creation failed")
        }
        ApplicationStatusFailureCode::InvalidAssignment => (
            "InvalidAssignment",
            "The hosting node could not realize the application assignment",
        ),
        ApplicationStatusFailureCode::TargetMismatch => (
            "TargetMismatch",
            "The assigned execution target does not match the hosting node's configuration",
        ),
        ApplicationStatusFailureCode::LinkedRouteUnknown => {
            ("LinkedRouteUnknown", "Linked inference route is unknown")
        }
    };
    Some(ApplicationDeploymentFailure {
        code: code.to_owned(),
        message: message.to_owned(),
        diagnostics: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::application::{
        private::v1::{ApplicationStatusFailureCode, ApplicationStatusStage},
        v1::ApplicationDeploymentState,
    };

    use super::{public_failure, stage_text, status_failure_code, status_stage};

    #[test]
    fn every_typed_stage_round_trips_through_its_recorded_text() {
        for value in 1..=33 {
            let stage = ApplicationStatusStage::try_from(value).expect("typed stage");
            let text = stage_text(stage).expect("recorded text");
            assert_eq!(status_stage(text), stage, "{text}");
        }
        assert_eq!(stage_text(ApplicationStatusStage::Unspecified), None);
        assert_eq!(
            status_stage("free text"),
            ApplicationStatusStage::Unspecified
        );
    }

    #[test]
    fn manager_hosted_failure_summaries_round_trip_through_their_codes() {
        for code in [
            ApplicationStatusFailureCode::RecoveryRequired,
            ApplicationStatusFailureCode::LaunchIdentityUnresolved,
            ApplicationStatusFailureCode::ImageMissing,
            ApplicationStatusFailureCode::ArtifactPreparationFailed,
            ApplicationStatusFailureCode::PublishedPortInUse,
            ApplicationStatusFailureCode::CreationFailed,
        ] {
            let failure = public_failure(code).expect("failure summary");
            let state = ApplicationDeploymentState {
                failure: Some(failure),
                ..Default::default()
            };
            assert_eq!(status_failure_code(&state), code);
        }
        assert_eq!(
            public_failure(ApplicationStatusFailureCode::CreationFailed)
                .expect("creation failure")
                .message,
            "Application workload creation failed"
        );
        assert_eq!(
            public_failure(ApplicationStatusFailureCode::ArtifactPreparationFailed)
                .expect("artifact preparation failure")
                .message,
            "Application image did not match its pinned identity or this node's platform"
        );
        let uncoded_artifact = ApplicationDeploymentState {
            stage: "artifact-preparation".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            status_failure_code(&uncoded_artifact),
            ApplicationStatusFailureCode::Unspecified
        );
    }
}
