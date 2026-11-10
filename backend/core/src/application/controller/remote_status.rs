//! Accepts node application status into the manager's current application records.
//!
//! Each report is answered entry by entry under the site guard: cleanup evidence finalizes only
//! the exact teardown placed on the reporting node, and a deployment status is recorded only for
//! the generation and launch the manager currently declares, so a late report never revives an
//! earlier state. For example a Ready report at generation 3 from the view revision that
//! assigned the current route name becomes Ready with its published port on the SiteNode's host,
//! such as `http://10.0.0.2:41000`, while the same report from before a route rename leaves the
//! deployment reconciling.

use std::collections::BTreeMap;

use dotcore_domain::deployment::DeploymentOwner;
use dotcore_proto::dotcore::application::{
    private::v1::{
        ApplicationCleanupDisposition, ApplicationCleanupReceipt,
        ApplicationControllerStatusSection, ApplicationDeploymentStatus,
        ApplicationLifecycleObservation, ApplicationStatusFailureCode, ApplicationStatusStage,
    },
    v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ApplicationDeploymentState, ApplicationDeploymentStep, ApplicationDeploymentSteps,
        ApplicationEndpoint, ApplicationLifecycleState, ApplicationReadinessState,
        CurrentApplicationDeployment,
    },
};
use dotcore_site::{
    repository::{DeploymentTeardownTransaction, SiteRepository, SiteRepositoryError},
    store::{FileCurrentState, StoreError},
    view::{
        cleanup_evidence::{classify_cleanup_evidence, finalize_cleanup_decisions},
        contract::application_cleanup_receipt,
        fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    },
};
use tracing::{debug, info, warn};

use super::{settled_state, status_text};
use crate::application::repository::{
    ApplicationRepositoryError, CurrentApplicationRepository, validate_declaration,
};

/// Stage recorded while the node has not reported the current launch of a deployment.
const RECONCILING_STAGE: &str = "reconciling";

/// One authenticated node application status report for the node view it names.
pub(super) struct ApplicationStatusReport<'a> {
    pub(super) node_id: &'a str,
    pub(super) projection: &'a NodeViewProjection,
    pub(super) application: &'a ApplicationControllerStatusSection,
    pub(super) manager_session: &'a ManagerSession,
    pub(super) node_incarnation: &'a NodeIncarnation,
    pub(super) status_sequence: u64,
}

/// Accepts one validated report and answers each cleanup evidence entry. A storage failure
/// leaves every entry rejected, so the node reports it again, and never affects model status.
pub(super) fn accept_application_status(
    site: &SiteRepository,
    state: &FileCurrentState,
    report: &ApplicationStatusReport<'_>,
) -> Vec<ApplicationCleanupReceipt> {
    let acknowledged = site
        .with_deployment_teardowns(|teardowns| accept_locked(teardowns, state, report))
        .map_err(AcceptanceError::Site)
        .and_then(|result| result)
        .unwrap_or_else(|error| {
            warn!(event_kind = "manager_application_status", outcome = "retryable", node_id = report.node_id, status_sequence = report.status_sequence, error = %error, "Manager could not record node application status; cleanup evidence stays unacknowledged");
            vec![false; report.application.cleanup_evidence.len()]
        });
    report
        .application
        .cleanup_evidence
        .iter()
        .zip(acknowledged)
        .map(|(evidence, acknowledged)| {
            application_cleanup_receipt(
                report.manager_session,
                report.node_incarnation,
                report.status_sequence,
                evidence,
                if acknowledged {
                    ApplicationCleanupDisposition::Acknowledged
                } else {
                    ApplicationCleanupDisposition::Rejected
                },
            )
        })
        .collect()
}

/// Storage failure while accepting one application status report.
#[derive(Debug, thiserror::Error)]
enum AcceptanceError {
    #[error("site guard could not be taken for node application status")]
    Site(#[source] SiteRepositoryError),

    #[error("manager lifecycle records could not be updated for node application status")]
    Lifecycle(#[source] StoreError),

    #[error("manager application records could not be updated for node application status")]
    Current(#[source] ApplicationRepositoryError),
}

/// Classifies cleanup evidence, then records statuses, then records node-resolved launch
/// identities, all under the site guard (site, then current state). Returns whether each cleanup
/// evidence entry is acknowledged.
fn accept_locked(
    teardowns: &mut DeploymentTeardownTransaction<'_>,
    state: &FileCurrentState,
    report: &ApplicationStatusReport<'_>,
) -> Result<Vec<bool>, AcceptanceError> {
    let decisions = report
        .application
        .cleanup_evidence
        .iter()
        .map(|evidence| {
            classify_cleanup_evidence(
                teardowns,
                state,
                DeploymentOwner::Application,
                report.node_id,
                &evidence.application_deployment_id,
                evidence.lifecycle_generation,
            )
            .map(|decision| (evidence.application_deployment_id.as_str(), decision))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(AcceptanceError::Lifecycle)?;
    let finalized = finalize_cleanup_decisions(
        teardowns,
        DeploymentOwner::Application,
        report.node_id,
        &decisions,
    );
    if let Some(finalized) = &finalized {
        // The declaration is durably gone; its current record goes next, then its lifecycle.
        for deployment_id in finalized {
            remove_current_record(state, deployment_id)?;
        }
        state
            .prune_lifecycle_records(DeploymentOwner::Application, finalized)
            .map_err(AcceptanceError::Lifecycle)?;
        for deployment_id in finalized {
            info!(
                event_kind = "application_cleanup_acknowledged",
                deployment_id,
                node_id = report.node_id,
                "Node application cleanup evidence acknowledged after durable teardown finalization"
            );
        }
    }
    let durable = finalized.is_some();

    let assignments = report
        .projection
        .application_assignments()
        .iter()
        .filter_map(|assignment| assignment.declaration.as_ref())
        .map(|declaration| (declaration.application_deployment_id.as_str(), declaration))
        .collect::<BTreeMap<_, _>>();
    let host = teardowns
        .site_node(report.node_id)
        .map(|node| node.host.clone());
    let mut retained = CurrentApplicationRepository::snapshot(state)
        .map_err(AcceptanceError::Current)?
        .deployments
        .into_iter()
        .map(|record| (record.application_deployment_id.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let mut pinned = Vec::new();
    for status in &report.application.deployments {
        let Some(declared) = declared_status_target(teardowns, &assignments, report, status) else {
            continue;
        };
        let previous = retained
            .get(&status.application_deployment_id)
            .and_then(|record| record.state.clone());
        if let Some(record) = status_record(
            status,
            declared,
            host.as_deref(),
            retained.get(&status.application_deployment_id),
        ) && commit_record(state, &mut retained, record.clone())?
        {
            log_transition(report.node_id, previous.as_ref(), &record);
        }
        if let Some(declaration) = with_reported_pins(declared, status, report.node_id) {
            pinned.push(declaration);
        }
    }
    record_pins(teardowns, pinned, report.node_id);

    Ok(decisions
        .iter()
        .map(|(_, decision)| decision.acknowledged(durable))
        .collect())
}

/// Returns the accepted declaration a status reports on when it matches the reported view and
/// the site's current intent: the same assignment, the same lifecycle generation, and a
/// placement on the reporting node. A status for an earlier generation is ignored, so it never
/// replaces the record a later mutation wrote.
fn declared_status_target<'a>(
    teardowns: &'a DeploymentTeardownTransaction<'_>,
    assignments: &BTreeMap<&str, &ApplicationDeploymentDeclaration>,
    report: &ApplicationStatusReport<'_>,
    status: &ApplicationDeploymentStatus,
) -> Option<&'a ApplicationDeploymentDeclaration> {
    let deployment_id = status.application_deployment_id.as_str();
    let assigned = assignments.get(deployment_id).is_some_and(|assigned| {
        assigned.lifecycle_generation == status.lifecycle_generation
            && assigned.placement == status.placement
    });
    let declared = teardowns
        .application_deployment(deployment_id)
        .filter(|declared| {
            declared.lifecycle_generation == status.lifecycle_generation
                && declared
                    .placement
                    .as_ref()
                    .is_some_and(|placement| placement.site_node_id == report.node_id)
        });
    if !assigned || declared.is_none() {
        debug!(
            event_kind = "manager_application_status",
            outcome = "ignored",
            node_id = report.node_id,
            deployment_id,
            lifecycle_generation = status.lifecycle_generation,
            "Manager ignored a node application status that does not match current intent"
        );
    }
    declared.filter(|_| assigned)
}

/// Builds the record a status stands for, or `None` when the retained record already says it.
///
/// A status of the current launch is recorded as observed. Until the node reports that
/// launch, an observation it recorded for an earlier launch of this generation, such as Ready
/// before a linked route rename, is replaced by a reconciling record; an intent record written
/// by an accepted mutation is kept.
fn status_record(
    status: &ApplicationDeploymentStatus,
    declared: &ApplicationDeploymentDeclaration,
    host: Option<&str>,
    retained: Option<&CurrentApplicationDeployment>,
) -> Option<CurrentApplicationDeployment> {
    let previous = retained.and_then(|record| record.state.as_ref());
    let generation = declared.lifecycle_generation;
    let (state, reconciled) = if reports_current_launch(status) {
        (
            observed_state(status, declared, host, previous),
            Some(generation),
        )
    } else if retained.is_some_and(|record| record.reconciled_lifecycle_generation.is_some()) {
        (reconciling_state(previous), None)
    } else {
        return None;
    };
    let unchanged_generation =
        retained.is_some_and(|record| record.reconciled_lifecycle_generation == reconciled);
    let mut state = match settled_state(previous, state) {
        Some(state) => state,
        None if unchanged_generation => return None,
        None => previous.cloned()?,
    };
    if previous.is_none() {
        state.started_at = Some(super::now());
        state.updated_at = state.started_at;
    }
    Some(CurrentApplicationDeployment {
        application_deployment_id: declared.application_deployment_id.clone(),
        record_revision: 0,
        state: Some(state),
        reconciled_lifecycle_generation: reconciled,
    })
}

/// Whether a status reports the launch of the assignment the manager currently publishes.
///
/// The node reports the view revision it reconciled only while that is the assignment of the
/// view the status names, which is the current published view. An assignment the node cannot
/// realize is reported as a typed failure without being reconciled.
fn reports_current_launch(status: &ApplicationDeploymentStatus) -> bool {
    let lifecycle = ApplicationLifecycleObservation::try_from(status.lifecycle)
        .unwrap_or(ApplicationLifecycleObservation::Unspecified);
    let unrealizable = matches!(
        ApplicationStatusFailureCode::try_from(status.failure_code),
        Ok(ApplicationStatusFailureCode::InvalidAssignment
            | ApplicationStatusFailureCode::TargetMismatch
            | ApplicationStatusFailureCode::LinkedRouteUnknown)
    );
    lifecycle != ApplicationLifecycleObservation::Reconciling
        && lifecycle != ApplicationLifecycleObservation::Unspecified
        && (status.accepted_view_revision != 0 || unrealizable)
}

/// Maps one reported observation to the state the owner records: the typed stage and failure
/// back to the public text the hosting node's realizer recorded, and the published endpoint's
/// port to a URL on `host`, the address the manager reaches the reporting SiteNode at.
fn observed_state(
    status: &ApplicationDeploymentStatus,
    declared: &ApplicationDeploymentDeclaration,
    host: Option<&str>,
    previous: Option<&ApplicationDeploymentState>,
) -> ApplicationDeploymentState {
    let observation = ApplicationLifecycleObservation::try_from(status.lifecycle)
        .unwrap_or(ApplicationLifecycleObservation::Unspecified);
    let lifecycle = public_lifecycle(observation);
    let failure_code = ApplicationStatusFailureCode::try_from(status.failure_code)
        .unwrap_or(ApplicationStatusFailureCode::Unspecified);
    let stage = ApplicationStatusStage::try_from(status.stage)
        .ok()
        .and_then(status_text::stage_text)
        .unwrap_or_else(|| default_stage(lifecycle, failure_code))
        .to_owned();
    let readiness = ApplicationReadinessState::try_from(status.readiness)
        .unwrap_or(ApplicationReadinessState::NotReady);
    let endpoint = status
        .endpoint
        .as_ref()
        .zip(host)
        .map(|(endpoint, host)| {
            // A node publishes on its bind address, which differs from an address `host` when
            // the path between them translates addresses; the manager cannot tell the two apart.
            // A host name always differs from the published address, so it says nothing.
            if endpoint.host != host && host.parse::<std::net::Ipv4Addr>().is_ok() {
                debug!(event_kind = "manager_application_status", outcome = "endpoint_translated", deployment_id = %status.application_deployment_id, published_host = %endpoint.host, node_host = host, port = endpoint.port, "Node published an application endpoint on another host; projecting it on the node host");
            }
            endpoint_url(host, endpoint.port)
        });
    // The node decides its start steps; a lifecycle outside a start never shows them, and steps
    // this manager cannot show are dropped without rejecting the status.
    let steps = status.steps.clone().filter(|steps| {
        matches!(
            lifecycle,
            ApplicationLifecycleState::Preparing | ApplicationLifecycleState::Starting
        ) && valid_steps(steps)
    });
    let observed_placement = match lifecycle {
        ApplicationLifecycleState::Stopped | ApplicationLifecycleState::Queued => None,
        ApplicationLifecycleState::Ready | ApplicationLifecycleState::Paused => {
            declared.placement.clone()
        }
        _ => previous.and_then(|state| state.observed_placement.clone()),
    };
    ApplicationDeploymentState {
        lifecycle_state: lifecycle as i32,
        readiness_state: readiness as i32,
        stage,
        started_at: previous.and_then(|state| state.started_at),
        updated_at: previous.and_then(|state| state.updated_at),
        failure: status_text::public_failure(failure_code),
        services: status.services.clone(),
        endpoint,
        observed_placement,
        observation_stale: false,
        steps,
    }
}

/// The record shown while the node has not reported the current launch: starting, without
/// readiness, services, endpoint, or start steps. Projection shows it as pausing or stopping when that is the
/// declared intent.
fn reconciling_state(previous: Option<&ApplicationDeploymentState>) -> ApplicationDeploymentState {
    let mut state = super::starting_state(previous);
    state.stage = RECONCILING_STAGE.to_owned();
    state.failure = None;
    state.steps = None;
    state
}

/// Whether reported start steps name only known steps and a current step among them.
fn valid_steps(steps: &ApplicationDeploymentSteps) -> bool {
    (steps.current as usize) < steps.steps.len()
        && steps.steps.iter().all(|step| {
            ApplicationDeploymentStep::try_from(*step)
                .is_ok_and(|step| step != ApplicationDeploymentStep::Unspecified)
        })
}

fn public_lifecycle(observation: ApplicationLifecycleObservation) -> ApplicationLifecycleState {
    match observation {
        ApplicationLifecycleObservation::Queued => ApplicationLifecycleState::Queued,
        ApplicationLifecycleObservation::Preparing => ApplicationLifecycleState::Preparing,
        ApplicationLifecycleObservation::Starting
        | ApplicationLifecycleObservation::Reconciling
        | ApplicationLifecycleObservation::Unspecified => ApplicationLifecycleState::Starting,
        ApplicationLifecycleObservation::Ready => ApplicationLifecycleState::Ready,
        ApplicationLifecycleObservation::Pausing => ApplicationLifecycleState::Pausing,
        ApplicationLifecycleObservation::Paused => ApplicationLifecycleState::Paused,
        ApplicationLifecycleObservation::Stopping => ApplicationLifecycleState::Stopping,
        ApplicationLifecycleObservation::Stopped => ApplicationLifecycleState::Stopped,
        ApplicationLifecycleObservation::Unavailable => ApplicationLifecycleState::Unavailable,
    }
}

/// Stage text for a status without a typed stage, such as an assignment the node rejected.
fn default_stage(
    lifecycle: ApplicationLifecycleState,
    failure_code: ApplicationStatusFailureCode,
) -> &'static str {
    match failure_code {
        ApplicationStatusFailureCode::InvalidAssignment
        | ApplicationStatusFailureCode::TargetMismatch => "target-compatibility",
        ApplicationStatusFailureCode::LinkedRouteUnknown => "launch-preparation",
        _ => match lifecycle {
            ApplicationLifecycleState::Queued => "queued",
            ApplicationLifecycleState::Ready => "ready",
            ApplicationLifecycleState::Pausing => "pausing",
            ApplicationLifecycleState::Paused => "paused",
            ApplicationLifecycleState::Stopping => "stopping",
            ApplicationLifecycleState::Stopped => "stopped",
            ApplicationLifecycleState::Unavailable => "unavailable",
            _ => "starting",
        },
    }
}

fn endpoint_url(host: &str, port: u32) -> ApplicationEndpoint {
    ApplicationEndpoint {
        url: format!("http://{host}:{port}"),
    }
}

/// Writes one changed record at the retained revision and reports whether it was written. A
/// record a concurrent owner mutation changed first is left to the next report.
fn commit_record(
    state: &FileCurrentState,
    retained: &mut BTreeMap<String, CurrentApplicationDeployment>,
    record: CurrentApplicationDeployment,
) -> Result<bool, AcceptanceError> {
    let deployment_id = record.application_deployment_id.clone();
    let expected = retained
        .get(&deployment_id)
        .map(|retained| retained.record_revision);
    match CurrentApplicationRepository::upsert(state, record.clone(), expected) {
        Ok(mutation) => {
            retained.insert(
                deployment_id,
                CurrentApplicationDeployment {
                    record_revision: mutation.record_revision,
                    ..record
                },
            );
            Ok(true)
        }
        Err(
            ApplicationRepositoryError::RevisionConflict
            | ApplicationRepositoryError::AlreadyExists
            | ApplicationRepositoryError::NotFound,
        ) => {
            debug!(
                event_kind = "manager_application_status",
                outcome = "superseded",
                deployment_id,
                "A concurrent owner mutation changed the record first; the next node status records it"
            );
            Ok(false)
        }
        Err(error) => Err(AcceptanceError::Current(error)),
    }
}

/// Reports a recorded node lifecycle or stage change; unchanged observations stay silent.
fn log_transition(
    node_id: &str,
    previous: Option<&ApplicationDeploymentState>,
    record: &CurrentApplicationDeployment,
) {
    let Some(next) = record.state.as_ref() else {
        return;
    };
    if previous.is_some_and(|previous| {
        previous.lifecycle_state == next.lifecycle_state && previous.stage == next.stage
    }) {
        return;
    }
    info!(
        event_kind = "application_lifecycle_transition",
        deployment_id = %record.application_deployment_id,
        node_id,
        previous_lifecycle_state = previous.map(|state| state.lifecycle_state),
        lifecycle_state = next.lifecycle_state,
        stage = %next.stage,
        reconciled_lifecycle_generation = record.reconciled_lifecycle_generation,
        "Node-hosted application deployment state recorded"
    );
}

fn remove_current_record(
    state: &FileCurrentState,
    deployment_id: &str,
) -> Result<(), AcceptanceError> {
    let retained = CurrentApplicationRepository::snapshot(state)
        .map_err(AcceptanceError::Current)?
        .deployments
        .into_iter()
        .find(|record| record.application_deployment_id == deployment_id);
    let Some(retained) = retained else {
        return Ok(());
    };
    match CurrentApplicationRepository::remove(state, deployment_id, retained.record_revision) {
        Ok(_)
        | Err(
            ApplicationRepositoryError::NotFound | ApplicationRepositoryError::RevisionConflict,
        ) => Ok(()),
        Err(error) => Err(AcceptanceError::Current(error)),
    }
}

/// Returns the declaration with the launch identities the node resolved and has not seen
/// projected yet, or `None` when it reports nothing new. A pin is recorded once: a report that
/// contradicts a projected pin, or that the declaration does not admit, is ignored.
fn with_reported_pins(
    declared: &ApplicationDeploymentDeclaration,
    status: &ApplicationDeploymentStatus,
    node_id: &str,
) -> Option<ApplicationDeploymentDeclaration> {
    if declared.desired_state == ApplicationDeploymentDesiredState::Teardown as i32 {
        return None;
    }
    let reported = dotcore_domain::deployment::launch_pin::record_reported_pins(
        &declared.node_resolved_launch_identities,
        &status.resolved_launch_identities,
    );
    for contradiction in &reported.contradicted {
        warn!(event_kind = "manager_application_launch_identity", outcome = "contradicted", deployment_id = %declared.application_deployment_id, node_id, selection_key = %contradiction.reported.selection_key, projected = %contradiction.recorded.launch_identity, reported = %contradiction.reported.launch_identity, "Node reported a launch identity that contradicts the recorded pin; the recorded pin is kept");
    }
    let pins = reported.recorded?;
    let declaration = ApplicationDeploymentDeclaration {
        node_resolved_launch_identities: pins,
        ..declared.clone()
    };
    match validate_declaration(&declaration) {
        Ok(()) => Some(declaration),
        Err(error) => {
            // The node reports it again with every status, so this stays at debug level.
            debug!(event_kind = "manager_application_launch_identity", outcome = "rejected", deployment_id = %declared.application_deployment_id, node_id, error = %error, "Node reported launch identities the declaration does not admit");
            None
        }
    }
}

/// Records node-resolved launch identities in their declarations and makes them durable on
/// their own, so a failed write never delays cleanup acknowledgement; the node keeps reporting
/// them until they are projected back.
fn record_pins(
    teardowns: &mut DeploymentTeardownTransaction<'_>,
    pinned: Vec<ApplicationDeploymentDeclaration>,
    node_id: &str,
) {
    if pinned.is_empty() {
        return;
    }
    let deployments = pinned
        .iter()
        .map(|declaration| declaration.application_deployment_id.clone())
        .collect::<Vec<_>>();
    for declaration in pinned {
        teardowns.replace_application_deployment(declaration);
    }
    match teardowns.make_durable() {
        Ok(()) => {
            info!(event_kind = "manager_application_launch_identity", outcome = "recorded", node_id, deployments = ?deployments, "Manager recorded node-resolved application launch identities")
        }
        Err(error) => {
            warn!(event_kind = "manager_application_launch_identity", outcome = "retryable", node_id, error = %error, "Manager could not record node-resolved application launch identities")
        }
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::deployment::DeploymentOwner;
    use dotcore_domain::test_support::oci::ServiceArtifactOci as _;
    use dotcore_domain::test_support::tracing::{Record, capture_records};
    use dotcore_proto::dotcore::{
        application::{
            private::v1::{
                ApplicationAssignment, ApplicationCleanupDisposition, ApplicationCleanupEvidence,
                ApplicationControllerStatusSection, ApplicationDeploymentStatus,
                ApplicationLifecycleObservation, ApplicationPublishedEndpoint,
                ApplicationStatusFailureCode, ApplicationStatusStage,
            },
            v1::{
                ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
                ApplicationDeploymentState, ApplicationDeploymentStep, ApplicationDeploymentSteps,
                ApplicationLifecycleState, ApplicationReadinessState,
                ApplicationServiceAcceleratorMemory, CurrentApplicationDeployment,
            },
        },
        deployment::v1::{
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation, AcceleratorMemoryUsageSet,
            ArtifactPreparationPolicy, ExactPlacement, NodeResolvedLaunchIdentity,
            accelerator_memory_observation,
        },
        site::v1::{ExecutionTarget, Site},
    };
    use dotcore_site::{
        repository::SiteRepository,
        store::FileCurrentState,
        view::{
            contract::application_cleanup_evidence,
            fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
        },
    };

    use super::super::test_support::{
        WORKER_HOST, application_execution_target, deployment, open_node_site_with, start_steps,
    };
    use super::{ApplicationStatusReport, accept_application_status};
    use crate::application::repository::{
        ApplicationDeclarationRepository, CurrentApplicationRepository,
    };

    const IMAGE: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn current_launch_status_is_recorded_with_public_text_and_its_node_endpoint() {
        let manager = Manager::new(vec![deployment(ApplicationDeploymentDesiredState::Present)]);
        manager.intent(super::super::queued_state());

        manager.accept(vec![ready(1, 7, WORKER_HOST)], Vec::new());
        let record = manager.record();
        let state = record.state.as_ref().expect("recorded state");
        assert_eq!(record.reconciled_lifecycle_generation, Some(1));
        assert_eq!(
            state.lifecycle_state,
            ApplicationLifecycleState::Ready as i32
        );
        assert_eq!(
            state.readiness_state,
            ApplicationReadinessState::Ready as i32
        );
        assert_eq!(state.stage, "ready");
        assert_eq!(
            state
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.url.as_str()),
            Some("http://10.0.0.2:41000")
        );
        assert_eq!(state.observed_placement, Some(node_placement()));
        assert!(state.started_at.is_some());

        let mut missing = status(1, ApplicationLifecycleObservation::Unavailable, 8);
        missing.stage = ApplicationStatusStage::EndpointNotReady as i32;
        missing.failure_code = ApplicationStatusFailureCode::ImageMissing as i32;
        manager.accept(vec![missing], Vec::new());
        let state = manager.record().state.expect("recorded state");
        assert_eq!(
            state.lifecycle_state,
            ApplicationLifecycleState::Unavailable as i32
        );
        assert_eq!(state.stage, "Application primary endpoint is not ready");
        let failure = state.failure.expect("failure summary");
        assert_eq!(
            (failure.code.as_str(), failure.message.as_str()),
            ("ImageMissing", "Application image is not available locally")
        );
    }

    #[test]
    fn reports_differing_only_in_unit_memory_leave_the_record_unwritten() {
        let manager = Manager::new(vec![deployment(ApplicationDeploymentDesiredState::Present)]);
        manager.intent(super::super::queued_state());
        let with_memory = |used_memory_bytes| {
            let mut reported = ready(1, 7, WORKER_HOST);
            reported.service_accelerator_memory = vec![ApplicationServiceAcceleratorMemory {
                service_name: "web".to_owned(),
                accelerator_memory: Some(AcceleratorMemoryObservation {
                    result: Some(accelerator_memory_observation::Result::Available(
                        AcceleratorMemoryUsageSet {
                            devices: vec![AcceleratorMemoryDeviceUsage {
                                managed_node_id: "node-one".to_owned(),
                                device_id: "GPU-a".to_owned(),
                                used_memory_bytes,
                            }],
                        },
                    )),
                    ..Default::default()
                }),
            }];
            reported
        };

        manager.accept(vec![with_memory(1000)], Vec::new());
        let before = manager.record();
        manager.accept(vec![with_memory(2000)], Vec::new());
        let after = manager.record();

        assert_eq!(after.record_revision, before.record_revision);
        assert_eq!(
            after.state.as_ref().and_then(|state| state.updated_at),
            before.state.as_ref().and_then(|state| state.updated_at)
        );
        assert_eq!(after, before);
    }

    #[test]
    fn booting_node_deployment_is_recorded_as_starting_with_its_boot_stage() {
        let manager = Manager::new(vec![deployment(ApplicationDeploymentDesiredState::Present)]);
        manager.intent(super::super::queued_state());

        let mut booting = status(1, ApplicationLifecycleObservation::Starting, 7);
        booting.stage = ApplicationStatusStage::EndpointNotReady as i32;
        manager.accept(vec![booting], Vec::new());

        let state = manager.record().state.expect("recorded state");
        assert_eq!(
            state.lifecycle_state,
            ApplicationLifecycleState::Starting as i32
        );
        assert_eq!(
            state.readiness_state,
            ApplicationReadinessState::NotReady as i32
        );
        assert_eq!(state.stage, "Application primary endpoint is not ready");
        assert_eq!(state.endpoint, None);
        assert_eq!(state.failure, None);
    }

    #[test]
    fn node_start_steps_and_preparing_are_recorded_until_a_reconciling_record_drops_them() {
        let manager = Manager::new(vec![deployment(ApplicationDeploymentDesiredState::Present)]);
        manager.intent(super::super::queued_state());

        let mut preparing = status(1, ApplicationLifecycleObservation::Preparing, 7);
        preparing.stage = ApplicationStatusStage::Preparing as i32;
        preparing.steps = Some(start_steps(0));
        manager.accept(vec![preparing], Vec::new());
        let state = manager.record().state.expect("recorded state");
        assert_eq!(
            state.lifecycle_state,
            ApplicationLifecycleState::Preparing as i32
        );
        assert_eq!(state.stage, "preparing");
        assert_eq!(state.steps, Some(start_steps(0)));

        let mut booting = status(1, ApplicationLifecycleObservation::Starting, 7);
        booting.stage = ApplicationStatusStage::EndpointNotReady as i32;
        booting.steps = Some(start_steps(2));
        manager.accept(vec![booting], Vec::new());
        assert_eq!(
            manager.record().state.expect("recorded state").steps,
            Some(start_steps(2))
        );

        // Steps the manager cannot show are dropped while the status is still recorded.
        for invalid in [
            ApplicationDeploymentSteps {
                current: 3,
                ..start_steps(0)
            },
            ApplicationDeploymentSteps {
                steps: vec![ApplicationDeploymentStep::Unspecified as i32],
                current: 0,
            },
        ] {
            let mut booting = status(1, ApplicationLifecycleObservation::Starting, 7);
            booting.stage = ApplicationStatusStage::GroupNotRunning as i32;
            booting.steps = Some(invalid);
            manager.accept(vec![booting], Vec::new());
            let state = manager.record().state.expect("recorded state");
            assert_eq!(state.stage, "Application group is not running");
            assert_eq!(state.steps, None);
        }

        // Steps outside a start are never recorded, whatever the node sent.
        let mut ready = ready(1, 7, WORKER_HOST);
        ready.steps = Some(start_steps(2));
        manager.accept(vec![ready], Vec::new());
        assert_eq!(manager.record().state.expect("ready state").steps, None);

        let mut booting = status(1, ApplicationLifecycleObservation::Starting, 7);
        booting.stage = ApplicationStatusStage::GroupNotRunning as i32;
        booting.steps = Some(start_steps(2));
        manager.accept(vec![booting.clone()], Vec::new());
        booting.accepted_view_revision = 0;
        manager.accept(vec![booting], Vec::new());
        let reconciling = manager.record().state.expect("reconciling state");
        assert_eq!(reconciling.stage, "reconciling");
        assert_eq!(reconciling.steps, None);
    }

    #[test]
    fn endpoint_published_on_the_node_bind_address_is_composed_on_its_site_node_host() {
        let manager = Manager::new(vec![deployment(ApplicationDeploymentDesiredState::Present)]);
        manager.intent(super::super::queued_state());

        let (_, records, _) =
            capture_records(|| manager.accept(vec![ready(1, 7, "127.0.0.2")], Vec::new()));
        let state = manager.record().state.expect("recorded state");
        assert!(records.iter().any(is_endpoint_translated));

        assert_eq!(
            state.lifecycle_state,
            ApplicationLifecycleState::Ready as i32
        );
        assert_eq!(
            state
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.url.as_str()),
            Some("http://10.0.0.2:41000")
        );
    }

    #[test]
    fn manager_hosted_endpoint_is_composed_on_the_declared_manager_host_name() {
        let placement = ExactPlacement {
            site_node_id: "manager-node".to_owned(),
            execution_target_id: "manager-target".to_owned(),
        };
        let mut hosted = deployment(ApplicationDeploymentDesiredState::Present);
        hosted.placement = Some(placement.clone());
        hosted.snapshot.as_mut().expect("snapshot").placement = Some(placement.clone());
        let manager = Manager::with_site(|site| {
            site.nodes[0].host = "manager.example.ts.net".to_owned();
            site.application_deployments = vec![hosted];
        });
        manager.intent(super::super::queued_state());
        let mut manager_target = application_execution_target();
        manager_target.execution_target_id = "manager-target".to_owned();
        manager_target.site_node_id = "manager-node".to_owned();

        let mut report = ready(1, 7, "100.64.0.7");
        report.placement = Some(placement);
        let (_, records, _) = capture_records(|| {
            manager.accept_from(
                "manager-node",
                manager_target,
                vec![manager.declaration()],
                vec![report],
                Vec::new(),
            )
        });

        let state = manager.record().state.expect("recorded state");
        assert!(!records.iter().any(is_endpoint_translated));
        assert_eq!(
            state.lifecycle_state,
            ApplicationLifecycleState::Ready as i32
        );
        assert_eq!(
            state
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.url.as_str()),
            Some("http://manager.example.ts.net:41000")
        );
    }

    #[test]
    fn older_generation_or_older_launch_status_never_revives_state() {
        let present = deployment(ApplicationDeploymentDesiredState::Present);
        let manager = Manager::new(vec![present.clone()]);
        manager.intent(super::super::queued_state());
        manager.accept(vec![ready(1, 7, WORKER_HOST)], Vec::new());

        // A linked route rename relaunches at the same generation: until the node reports the
        // new launch, the earlier Ready observation is replaced by a reconciling record.
        let mut earlier_launch = ready(1, 0, WORKER_HOST);
        earlier_launch.accepted_view_revision = 0;
        manager.accept(vec![earlier_launch.clone()], Vec::new());
        let reconciling = manager.record();
        assert_eq!(reconciling.reconciled_lifecycle_generation, None);
        let state = reconciling.state.as_ref().expect("reconciling state");
        assert_eq!(
            (state.lifecycle_state, state.stage.as_str()),
            (ApplicationLifecycleState::Starting as i32, "reconciling")
        );
        assert_eq!(state.endpoint, None);
        manager.accept(vec![earlier_launch], Vec::new());
        assert_eq!(
            manager.record(),
            reconciling,
            "unchanged evidence writes nothing"
        );

        // A pause accepted on the manager is never replaced by a status of the generation before.
        manager.pause(2);
        manager.accept_against(vec![present], vec![ready(1, 9, WORKER_HOST)], Vec::new());
        let paused_intent = manager.record();
        assert_eq!(
            paused_intent
                .state
                .as_ref()
                .expect("pausing")
                .lifecycle_state,
            ApplicationLifecycleState::Pausing as i32
        );
        // Neither does a status of the paused generation before the node reconciled it.
        manager.accept(
            vec![status(2, ApplicationLifecycleObservation::Reconciling, 0)],
            Vec::new(),
        );
        assert_eq!(manager.record(), paused_intent);

        let mut paused = status(2, ApplicationLifecycleObservation::Paused, 10);
        paused.stage = ApplicationStatusStage::Paused as i32;
        manager.accept(vec![paused], Vec::new());
        let record = manager.record();
        assert_eq!(record.reconciled_lifecycle_generation, Some(2));
        assert_eq!(
            record.state.expect("paused").lifecycle_state,
            ApplicationLifecycleState::Paused as i32
        );
    }

    #[test]
    fn node_resolved_launch_identities_are_recorded_once_and_projected_back() {
        let manager = Manager::new(vec![local_development_deployment()]);
        manager.intent(super::super::queued_state());
        let mut pinned = status(1, ApplicationLifecycleObservation::Starting, 3);
        pinned.stage = ApplicationStatusStage::Starting as i32;
        pinned.resolved_launch_identities = vec![pin(IMAGE)];

        manager.accept(vec![pinned.clone()], Vec::new());
        assert_eq!(
            manager.declaration().node_resolved_launch_identities,
            [pin(IMAGE)]
        );
        let on_disk = SiteRepository::load(
            &manager.path(),
            crate::test_support::site_declaration_rules(),
        )
        .expect("reloaded site");
        assert_eq!(
            on_disk.application_snapshot().expect("Site").deployments[0]
                .node_resolved_launch_identities,
            [pin(IMAGE)]
        );

        let mut contradicting = pinned;
        contradicting.lifecycle_generation = 1;
        contradicting.resolved_launch_identities = vec![pin(
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )];
        manager.accept_against(vec![manager.declaration()], vec![contradicting], Vec::new());
        assert_eq!(
            manager.declaration().node_resolved_launch_identities,
            [pin(IMAGE)],
            "a recorded pin is never replaced"
        );
    }

    #[test]
    fn cleanup_evidence_finalizes_only_the_exact_teardown_placed_on_the_reporting_node() {
        let mut teardown = deployment(ApplicationDeploymentDesiredState::Teardown);
        teardown.application_deployment_id = "gone-1".to_owned();
        teardown.lifecycle_generation = 4;
        let mut present = deployment(ApplicationDeploymentDesiredState::Present);
        present.lifecycle_generation = 3;
        let mut manager_hosted = deployment(ApplicationDeploymentDesiredState::Teardown);
        manager_hosted.application_deployment_id = "manager-hosted-1".to_owned();
        let placement = ExactPlacement {
            site_node_id: "manager-node".to_owned(),
            execution_target_id: "manager-target".to_owned(),
        };
        manager_hosted.placement = Some(placement.clone());
        manager_hosted
            .snapshot
            .as_mut()
            .expect("snapshot")
            .placement = Some(placement);
        let manager = Manager::new(vec![teardown, present, manager_hosted]);
        for _ in 0..4 {
            manager.reserve("gone-1");
        }
        for _ in 0..3 {
            manager.reserve("open-webui-1");
        }
        manager.intent_for("gone-1", super::super::stopping_state());

        let rejected = manager.accept(
            Vec::new(),
            vec![
                application_cleanup_evidence("gone-1", 5, Some(1)),
                application_cleanup_evidence("open-webui-1", 3, Some(1)),
                application_cleanup_evidence("manager-hosted-1", 1, None),
            ],
        );
        assert_eq!(rejected, [ApplicationCleanupDisposition::Rejected; 3]);
        assert!(manager.declared("gone-1") && manager.declared("manager-hosted-1"));

        let answered = manager.accept(
            Vec::new(),
            vec![
                application_cleanup_evidence("gone-1", 4, Some(1)),
                application_cleanup_evidence("open-webui-1", 2, Some(1)),
                application_cleanup_evidence("never-declared-1", 1, None),
            ],
        );
        assert_eq!(answered, [ApplicationCleanupDisposition::Acknowledged; 3]);
        assert!(!manager.declared("gone-1"));
        let on_disk = SiteRepository::load(
            &manager.path(),
            crate::test_support::site_declaration_rules(),
        )
        .expect("reloaded site");
        assert!(
            on_disk
                .application_snapshot()
                .expect("Site")
                .deployments
                .iter()
                .all(|declaration| declaration.application_deployment_id != "gone-1")
        );
        assert!(
            CurrentApplicationRepository::snapshot(&manager.state)
                .expect("current state")
                .deployments
                .iter()
                .all(|record| record.application_deployment_id != "gone-1")
        );
        assert_eq!(
            manager
                .state
                .lifecycle_high_water(DeploymentOwner::Application, "gone-1")
                .expect("lifecycle records"),
            None
        );
        assert_eq!(
            manager
                .state
                .lifecycle_high_water(DeploymentOwner::Application, "open-webui-1")
                .expect("lifecycle records"),
            Some(3),
            "obsolete evidence has no effect"
        );
        assert!(manager.declared("open-webui-1") && manager.declared("manager-hosted-1"));
    }

    /// Manager site with the node `node-one` and its current state, both on disk.
    struct Manager {
        directory: tempfile::TempDir,
        site: SiteRepository,
        state: FileCurrentState,
    }

    impl Manager {
        fn new(declarations: Vec<ApplicationDeploymentDeclaration>) -> Self {
            Self::with_site(|site| site.application_deployments = declarations)
        }

        /// Opens the node site after `customize` changes it.
        fn with_site(customize: impl FnOnce(&mut Site)) -> Self {
            let directory = crate::test_support::canonical_tempdir().expect("Site directory");
            let (site, state) = open_node_site_with(directory.path(), customize);
            Self {
                directory,
                site,
                state,
            }
        }

        fn path(&self) -> std::path::PathBuf {
            self.directory.path().join("site.yaml")
        }

        fn intent(&self, state: ApplicationDeploymentState) {
            self.intent_for("open-webui-1", state);
        }

        fn intent_for(&self, deployment_id: &str, state: ApplicationDeploymentState) {
            let existing = CurrentApplicationRepository::snapshot(&self.state)
                .expect("current state")
                .deployments
                .into_iter()
                .find(|record| record.application_deployment_id == deployment_id)
                .map(|record| record.record_revision);
            CurrentApplicationRepository::upsert(
                &self.state,
                CurrentApplicationDeployment {
                    application_deployment_id: deployment_id.to_owned(),
                    record_revision: 0,
                    state: Some(state),
                    reconciled_lifecycle_generation: None,
                },
                existing,
            )
            .expect("intent record");
        }

        fn reserve(&self, deployment_id: &str) {
            self.state
                .reserve_lifecycle_generation(DeploymentOwner::Application, deployment_id)
                .expect("reserved generation");
        }

        /// Accepts a pause at `generation` the way the owner does: site intent, then its record.
        fn pause(&self, generation: u64) {
            ApplicationDeclarationRepository::pause(&self.site, "open-webui-1", 0, generation)
                .expect("pause intent");
            let previous = self.record().state;
            self.intent(super::super::pausing_state(previous.as_ref()));
        }

        fn record(&self) -> CurrentApplicationDeployment {
            CurrentApplicationRepository::snapshot(&self.state)
                .expect("current state")
                .deployments
                .into_iter()
                .find(|record| record.application_deployment_id == "open-webui-1")
                .expect("current record")
        }

        fn declaration(&self) -> ApplicationDeploymentDeclaration {
            self.site
                .application_snapshot()
                .expect("Site")
                .deployments
                .into_iter()
                .find(|declaration| declaration.application_deployment_id == "open-webui-1")
                .expect("declaration")
        }

        fn declared(&self, deployment_id: &str) -> bool {
            self.site
                .application_snapshot()
                .expect("Site")
                .deployments
                .iter()
                .any(|declaration| declaration.application_deployment_id == deployment_id)
        }

        /// Accepts a report against a view assigning the current node declarations.
        fn accept(
            &self,
            statuses: Vec<ApplicationDeploymentStatus>,
            evidence: Vec<ApplicationCleanupEvidence>,
        ) -> Vec<ApplicationCleanupDisposition> {
            let assigned = self
                .site
                .application_snapshot()
                .expect("Site")
                .deployments
                .into_iter()
                .filter(|declaration| {
                    declaration
                        .placement
                        .as_ref()
                        .is_some_and(|placement| placement.site_node_id == "node-one")
                })
                .collect();
            self.accept_against(assigned, statuses, evidence)
        }

        fn accept_against(
            &self,
            assigned: Vec<ApplicationDeploymentDeclaration>,
            statuses: Vec<ApplicationDeploymentStatus>,
            evidence: Vec<ApplicationCleanupEvidence>,
        ) -> Vec<ApplicationCleanupDisposition> {
            self.accept_from(
                "node-one",
                application_execution_target(),
                assigned,
                statuses,
                evidence,
            )
        }

        /// Accepts a report from `node_id` against a view assigning `assigned` on `target`.
        fn accept_from(
            &self,
            node_id: &str,
            target: ExecutionTarget,
            assigned: Vec<ApplicationDeploymentDeclaration>,
            statuses: Vec<ApplicationDeploymentStatus>,
            evidence: Vec<ApplicationCleanupEvidence>,
        ) -> Vec<ApplicationCleanupDisposition> {
            let projection = NodeViewProjection::new(vec![target], Vec::new())
                .expect("node projection")
                .with_application_assignments(
                    assigned
                        .into_iter()
                        .map(|declaration| ApplicationAssignment {
                            declaration: Some(declaration),
                            linked_route: None,
                        })
                        .collect(),
                )
                .expect("application assignments");
            accept_application_status(
                &self.site,
                &self.state,
                &ApplicationStatusReport {
                    node_id,
                    projection: &projection,
                    application: &ApplicationControllerStatusSection {
                        deployments: statuses,
                        cleanup_evidence: evidence,
                    },
                    manager_session: &ManagerSession::parse("a".repeat(32)).expect("session"),
                    node_incarnation: &NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
                    status_sequence: 1,
                },
            )
            .into_iter()
            .map(|receipt| {
                ApplicationCleanupDisposition::try_from(receipt.disposition).expect("disposition")
            })
            .collect()
        }
    }

    fn node_placement() -> ExactPlacement {
        ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "application-target-one".to_owned(),
        }
    }

    fn status(
        lifecycle_generation: u64,
        lifecycle: ApplicationLifecycleObservation,
        accepted_view_revision: u64,
    ) -> ApplicationDeploymentStatus {
        ApplicationDeploymentStatus {
            application_deployment_id: "open-webui-1".to_owned(),
            lifecycle_generation,
            placement: Some(node_placement()),
            accepted_view_revision,
            state_revision: 1,
            lifecycle: lifecycle as i32,
            readiness: ApplicationReadinessState::NotReady as i32,
            ..Default::default()
        }
    }

    fn is_endpoint_translated(record: &Record) -> bool {
        matches!(record, Record::Event { fields, .. }
            if fields.get("outcome").map(String::as_str) == Some("\"endpoint_translated\""))
    }

    fn ready(
        lifecycle_generation: u64,
        accepted_view_revision: u64,
        host: &str,
    ) -> ApplicationDeploymentStatus {
        ApplicationDeploymentStatus {
            stage: ApplicationStatusStage::Ready as i32,
            readiness: ApplicationReadinessState::Ready as i32,
            realization_generation: Some(1),
            endpoint: Some(ApplicationPublishedEndpoint {
                host: host.to_owned(),
                port: 41_000,
            }),
            ..status(
                lifecycle_generation,
                ApplicationLifecycleObservation::Ready,
                accepted_view_revision,
            )
        }
    }

    fn pin(launch_identity: &str) -> NodeResolvedLaunchIdentity {
        NodeResolvedLaunchIdentity {
            selection_key: local_development_deployment()
                .snapshot
                .expect("snapshot")
                .service_selections[0]
                .service_name
                .clone(),
            launch_identity: launch_identity.to_owned(),
        }
    }

    /// The node deployment with a tag-only service left for its node to resolve.
    fn local_development_deployment() -> ApplicationDeploymentDeclaration {
        let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
        let snapshot = declaration.snapshot.as_mut().expect("snapshot");
        let reference = snapshot.graph.as_mut().expect("graph").services[0]
            .artifact
            .as_mut()
            .expect("artifact")
            .oci_mut()
            .expect("reference");
        reference.tag = Some("dev".to_owned());
        reference.digest = None;
        let selection = &mut snapshot.service_selections[0];
        selection.preparation_policy = ArtifactPreparationPolicy::LocalDevelopment as i32;
        selection.launch_identity.clear();
        declaration
    }
}
