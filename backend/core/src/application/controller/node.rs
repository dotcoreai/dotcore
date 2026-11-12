//! Node composition of the apps-controller realizer for the application deployments the
//! manager assigns to this SiteNode, and the typed application status this node reports.
//!
//! The realizer reads only the application section of the installed node view, takes linked
//! route names from it, and acts on the node's own application targets under the shared view
//! lease. Its durable node records hold ownership evidence, once-resolved development image
//! pins, and cleanup evidence; status reports them with typed stages and failure codes, for
//! example a deployment Ready at lifecycle generation 3 reconciled from view revision 12.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use dotcore_domain::{
    SiteNodeId, clock::Clock, deployment::launch_pin::LaunchPinResolver,
    inference_route::is_valid_route_name, timing::ApplicationTiming,
};
use dotcore_proto::dotcore::{
    application::{
        private::v1::{
            ApplicationAssignment, ApplicationControllerStatusSection, ApplicationDeploymentStatus,
            ApplicationLifecycleObservation, ApplicationPublishedEndpoint,
            ApplicationStatusFailureCode, ApplicationStatusStage,
            application_linked_route_projection,
        },
        v1::{
            ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
            ApplicationLifecycleState, ApplicationServiceAcceleratorMemory,
            CurrentApplicationDeployment,
        },
    },
    deployment::v1::AcceleratorMemoryObservation,
    site::{
        store::v1::NodeApplicationRealization,
        v1::{ExecutionTarget, execution_target},
    },
};
use dotcore_site::node::{
    agent::CollectionReading,
    hardware_inventory::{HardwareCollection, HardwareInventorySource},
};
use dotcore_site::{
    repository::ApplicationSiteSnapshot,
    store::{FileNodeState, StoreError},
    view::{
        contract::{application_cleanup_evidence, bounded_accelerator_memory},
        desired::NodeApplicationStatusOwner,
        fencing::{
            AcceptedApplicationOwnerInput, ActionAssignment, InstalledApplicationSection,
            ManagerSession,
        },
    },
};
use tokio::sync::Notify;
use tracing::warn;

use super::{
    ApplicationLifecycleError, ApplicationRealizationInput, ApplicationRealizationResults,
    ApplicationRealizationSnapshot, InferenceRouteResolutionError, InferenceRouteResolver,
    NodeRealizationRecords, RealizedView, ResolvedInferenceRoute,
    accelerator_memory::GroupAcceleratorAttribution,
    configuration,
    logs::NodeApplicationLogs,
    metrics::{HostedApplication, HostedApplicationResources, HostedService},
    outcome_uncertain_with_source,
    realizer::{ApplicationRealizer, NodeRealization, RealizerDependencies},
    resources::GroupResourceAttribution,
};
use crate::{
    application::repository::CurrentApplicationRepository,
    deployment_logs::{PreparationLogs, PreparationOwner, WorkloadLifecycle},
    dispatch::application::ApplicationRuntimeMap,
    model::state::CurrentStateError,
};

/// Node dependencies of the apps-controller realizer, all local to this SiteNode.
pub(crate) struct NodeApplicationComposition {
    /// Application section of the installed node view.
    pub(crate) input: AcceptedApplicationOwnerInput,

    /// Node store holding this node's current and realization records.
    pub(crate) state: FileNodeState,

    /// Runtime map of this node's own application targets, fenced by the view lease.
    pub(crate) runtimes: ApplicationRuntimeMap,

    /// Resolver pinning this node's development and locked service sources.
    pub(crate) launch_pins: Option<Arc<dyn LaunchPinResolver>>,

    /// Image preparation, present only when this node runs applications on Docker.
    pub(crate) artifact_manager: Option<dotcore_artifact::ArtifactManager>,

    /// Specific IPv4 address on which this node publishes application endpoints.
    pub(crate) publication_host: Ipv4Addr,

    /// Preparation logs the node keeps for its deployments, removed with their cleanup.
    pub(crate) preparation_logs: PreparationLogs,

    /// Hardware collections of this node, which its application resources are attributed from.
    pub(crate) inventory: Arc<dyn HardwareInventorySource>,

    /// Timing of the realizer's observer passes and of each deployment's startup.
    pub(crate) timing: ApplicationTiming,

    /// Backend clock the realizer's observer waits through.
    pub(crate) clock: Arc<dyn Clock>,
}

/// Composed node apps-controller realizer, the status owner reporting what it realized, and
/// the log owner reading what it launched.
///
/// Dropping it stops the realizer's observer; runtime groups it created keep running.
pub(crate) struct NodeApplicationRealizer {
    realizer: Arc<ApplicationRealizer>,
    status: Arc<NodeApplicationStatus>,
    logs: Arc<NodeApplicationLogs>,
}

impl NodeApplicationRealizer {
    /// Builds the node realizer; it acts only once a view is installed and its lease is fresh.
    pub(crate) fn new(
        composition: NodeApplicationComposition,
    ) -> Result<Self, ApplicationLifecycleError> {
        let NodeApplicationComposition {
            input,
            state,
            runtimes,
            launch_pins,
            artifact_manager,
            publication_host,
            preparation_logs,
            inventory,
            timing,
            clock,
        } = composition;
        if publication_host.is_unspecified() {
            return Err(configuration(
                "Application publication host must be a specific IPv4 address",
            ));
        }
        let view = Arc::new(NodeViewInput {
            input: input.clone(),
        });
        let records = Arc::new(DurableNodeRecords {
            state: state.clone(),
            preparation_logs: preparation_logs.clone(),
        });
        let reconciled = ReconciledAssignments::default();
        let lifecycle = WorkloadLifecycle::new();
        let logs = Arc::new(NodeApplicationLogs::new(
            input.clone(),
            Arc::clone(&records) as Arc<dyn NodeRealizationRecords>,
            Arc::clone(&view) as Arc<dyn InferenceRouteResolver>,
            runtimes.clone(),
            publication_host,
            preparation_logs.clone(),
            lifecycle.clone(),
        ));
        let accelerator_attribution = GroupAcceleratorAttribution::default();
        let resource_attribution =
            GroupResourceAttribution::new(inventory, Arc::new(runtimes.clone()));
        let realizer = ApplicationRealizer::new(
            Arc::clone(&view) as Arc<dyn ApplicationRealizationInput>,
            Arc::new(state.clone()),
            Arc::clone(&records) as Arc<dyn ApplicationRealizationResults>,
            RealizerDependencies {
                runtimes,
                publication_host,
                launch_pins,
                artifact_manager,
                preparation_logs,
                lifecycle,
                inference_route_resolver: view,
                node: NodeRealization {
                    records,
                    reconciled: reconciled.clone(),
                },
                accelerator_attribution: Some(accelerator_attribution.clone()),
                resource_attribution: Some(resource_attribution.clone()),
                timing,
                clock,
            },
        );
        Ok(Self {
            realizer: Arc::new(realizer),
            status: Arc::new(NodeApplicationStatus {
                input,
                state,
                reconciled,
                accelerator_memory: accelerator_attribution,
                resources: resource_attribution,
            }),
            logs,
        })
    }

    /// Starts the serialized observer that reconciles on view installation or cadence.
    pub(crate) fn start_observer(&self) {
        ApplicationRealizer::start_observer(&self.realizer);
    }

    /// Returns the owner of the application section of this node's status reports.
    pub(crate) fn status_owner(&self) -> Arc<dyn NodeApplicationStatusOwner> {
        Arc::clone(&self.status) as Arc<dyn NodeApplicationStatusOwner>
    }

    /// Returns the owner answering manager log reads for the application deployments this
    /// node is assigned.
    pub(crate) fn log_owner(&self) -> Arc<dyn crate::deployment_logs::DeploymentLogOwner> {
        Arc::clone(&self.logs) as Arc<dyn crate::deployment_logs::DeploymentLogOwner>
    }

    /// Returns the reading of every realized group's host resources, to take with each of the
    /// node's hardware collections.
    pub(crate) fn resource_reading(&self) -> Arc<dyn CollectionReading> {
        Arc::new(self.status.resources.clone())
    }

    /// Returns the resources this node's status attributes to its application services.
    pub(crate) fn hosted_resources(&self) -> Arc<dyn HostedApplicationResources> {
        Arc::clone(&self.status) as Arc<dyn HostedApplicationResources>
    }

    #[cfg(test)]
    pub(super) async fn reconcile_for_test(&self) -> Result<(), ApplicationLifecycleError> {
        self.realizer.reconcile().await.map(|_| ())
    }
}

impl Drop for NodeApplicationRealizer {
    fn drop(&mut self) {
        self.realizer.shutdown_token().cancel();
    }
}

/// Realization input and linked-route source backed by the installed view's application
/// section. Only assignments that classify as valid for this node are realized; the others are
/// reported as their own typed failures.
struct NodeViewInput {
    input: AcceptedApplicationOwnerInput,
}

impl ApplicationRealizationInput for NodeViewInput {
    fn realization_snapshot(
        &self,
    ) -> Result<ApplicationRealizationSnapshot, ApplicationLifecycleError> {
        let Some(installed) = self.input.installed() else {
            return Ok(ApplicationRealizationSnapshot {
                site: ApplicationSiteSnapshot {
                    site_id: String::new(),
                    nodes: Vec::new(),
                    execution_targets: Vec::new(),
                    deployments: Vec::new(),
                },
                assignments: BTreeMap::new(),
                view: None,
            });
        };
        let mut deployments = Vec::new();
        let mut assignments = BTreeMap::new();
        for assignment in &installed.assignments {
            if classify_application_assignment(
                &installed.node_id,
                &installed.execution_targets,
                assignment,
            )
            .is_err()
            {
                continue;
            }
            let declaration = assignment
                .declaration
                .clone()
                .expect("a classified assignment carries its declaration");
            assignments.insert(
                declaration.application_deployment_id.clone(),
                ActionAssignment::from(assignment),
            );
            deployments.push(declaration);
        }
        Ok(ApplicationRealizationSnapshot {
            site: ApplicationSiteSnapshot {
                site_id: installed.site_id,
                nodes: Vec::new(),
                execution_targets: installed.execution_targets,
                deployments,
            },
            assignments,
            view: Some(RealizedView {
                manager_session: installed.manager_session,
                revision: installed.view_revision,
            }),
        })
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        self.input.change_listener()
    }
}

impl InferenceRouteResolver for NodeViewInput {
    fn resolve_for_create(
        &self,
        _: &str,
        _: u64,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        Err(InferenceRouteResolutionError::Unavailable)
    }

    /// Projects the route name the manager sent with the assignments linking this route.
    fn project_current(
        &self,
        inference_route_id: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        let installed = self
            .input
            .installed()
            .ok_or(InferenceRouteResolutionError::Unavailable)?;
        installed
            .assignments
            .iter()
            .filter(|assignment| {
                assignment
                    .declaration
                    .as_ref()
                    .and_then(|declaration| declaration.inference_route_id.as_deref())
                    == Some(inference_route_id)
            })
            .find_map(|assignment| {
                match assignment
                    .linked_route
                    .as_ref()
                    .and_then(|linked| linked.resolution.as_ref())
                {
                    Some(application_linked_route_projection::Resolution::RouteName(name)) => {
                        Some(ResolvedInferenceRoute {
                            inference_route_id: inference_route_id.to_owned(),
                            route_name: name.clone(),
                        })
                    }
                    _ => None,
                }
            })
            .ok_or(InferenceRouteResolutionError::Unknown)
    }
}

/// Node records and cleanup results backed by the durable node store.
struct DurableNodeRecords {
    state: FileNodeState,
    preparation_logs: PreparationLogs,
}

impl NodeRealizationRecords for DurableNodeRecords {
    fn realization(
        &self,
        deployment_id: &str,
    ) -> Result<Option<NodeApplicationRealization>, ApplicationLifecycleError> {
        self.state
            .node_application_realization(deployment_id)
            .map_err(node_records_error)
    }

    fn record_realization(
        &self,
        realization: NodeApplicationRealization,
    ) -> Result<(), ApplicationLifecycleError> {
        self.state
            .record_node_application_realization(realization)
            .map_err(node_records_error)
    }

    fn cleanup_confirmed(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) -> Result<bool, ApplicationLifecycleError> {
        Ok(self
            .state
            .node_application_cleanup_evidence()
            .map_err(node_records_error)?
            .iter()
            .any(|evidence| {
                evidence.application_deployment_id == deployment_id
                    && evidence.lifecycle_generation == lifecycle_generation
            }))
    }
}

impl ApplicationRealizationResults for DurableNodeRecords {
    /// Removes the deployment's preparation logs, then its current and realization records and
    /// records its cleanup evidence in one state transaction; a log removal failure is reported
    /// but never blocks it. The manager finalizes the teardown on acknowledgement.
    fn confirm_cleanup(
        &self,
        teardown: &ApplicationDeploymentDeclaration,
    ) -> Result<(), ApplicationLifecycleError> {
        // A log that cannot be removed must not hold the teardown back; the removal still stops
        // every earlier attempt from writing to it.
        if let Err(error) = self.preparation_logs.remove(
            PreparationOwner::Application,
            &teardown.application_deployment_id,
        ) {
            warn!(
                event_kind = "preparation_log_removal_failed",
                deployment_id = %teardown.application_deployment_id,
                error = %error,
                "Preparation logs of a cleaned-up application deployment could not be removed"
            );
        }
        self.state
            .confirm_node_application_cleanup(
                &teardown.application_deployment_id,
                teardown.lifecycle_generation,
            )
            .map(drop)
            .map_err(node_records_error)
    }
}

/// The assignment and installed view each deployment last reconciled, so status reports the
/// view revision only while the installed assignment is still the one reconciled.
#[derive(Clone, Default)]
pub(super) struct ReconciledAssignments {
    reconciled: Arc<Mutex<BTreeMap<String, (ActionAssignment, RealizedView)>>>,
}

impl ReconciledAssignments {
    pub(super) fn record(
        &self,
        deployment_id: &str,
        assignment: &ActionAssignment,
        view: &RealizedView,
    ) {
        self.lock()
            .insert(deployment_id.to_owned(), (assignment.clone(), view.clone()));
    }

    /// Returns the view revision that reconciled exactly `assignment` under `manager_session`.
    fn revision(
        &self,
        deployment_id: &str,
        assignment: &ActionAssignment,
        manager_session: &ManagerSession,
    ) -> Option<u64> {
        self.lock()
            .get(deployment_id)
            .filter(|(reconciled, view)| {
                reconciled == assignment && &view.manager_session == manager_session
            })
            .map(|(_, view)| view.revision)
    }

    /// Forgets deployments the installed view no longer assigns.
    fn retain(&self, installed: &BTreeSet<&str>) {
        self.lock()
            .retain(|deployment_id, _| installed.contains(deployment_id.as_str()));
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<String, (ActionAssignment, RealizedView)>> {
        self.reconciled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Owner of the application section of this node's status reports.
struct NodeApplicationStatus {
    input: AcceptedApplicationOwnerInput,
    state: FileNodeState,
    reconciled: ReconciledAssignments,

    /// Attributes accelerator memory to the units of the groups this node observes, so its
    /// status reports carry the node's own projection.
    accelerator_memory: GroupAcceleratorAttribution,

    /// Attributes host CPU and memory to the units of the groups this node realizes.
    resources: GroupResourceAttribution,
}

impl NodeApplicationStatusOwner for NodeApplicationStatus {
    /// Builds one status per installed assignment and lists unacknowledged cleanup evidence.
    /// Evidence stands in for its teardown; an assignment that is invalid for this node reports
    /// its typed failure without a realization.
    fn status_section(
        &self,
        collection: &HardwareCollection,
    ) -> Option<ApplicationControllerStatusSection> {
        let installed = self.input.installed()?;
        let current = CurrentApplicationRepository::snapshot(&self.state)
            .inspect_err(|error| warn_status_not_built("current state", error))
            .ok()?;
        let realizations = self
            .state
            .node_application_realizations()
            .inspect_err(|error| warn_status_not_built("realization records", error))
            .ok()?;
        let evidence = self
            .state
            .node_application_cleanup_evidence()
            .inspect_err(|error| warn_status_not_built("cleanup evidence", error))
            .ok()?
            .into_iter()
            .filter(|evidence| !evidence.acknowledged)
            .collect::<Vec<_>>();
        let cleaned = evidence
            .iter()
            .map(|evidence| {
                (
                    evidence.application_deployment_id.as_str(),
                    evidence.lifecycle_generation,
                )
            })
            .collect::<BTreeSet<_>>();
        let records = current
            .deployments
            .iter()
            .map(|record| (record.application_deployment_id.as_str(), record))
            .collect::<BTreeMap<_, _>>();
        let realizations = realizations
            .iter()
            .map(|realization| (realization.application_deployment_id.as_str(), realization))
            .collect::<BTreeMap<_, _>>();
        self.reconciled.retain(
            &installed
                .assignments
                .iter()
                .filter_map(|assignment| assignment.declaration.as_ref())
                .map(|declaration| declaration.application_deployment_id.as_str())
                .collect(),
        );
        let deployments = installed
            .assignments
            .iter()
            .filter_map(|assignment| {
                let declaration = assignment.declaration.as_ref()?;
                let id = declaration.application_deployment_id.as_str();
                (!cleaned.contains(&(id, declaration.lifecycle_generation))).then(|| {
                    self.deployment_status(
                        &installed,
                        assignment,
                        declaration,
                        records.get(id).copied(),
                        realizations.get(id).copied(),
                        collection,
                    )
                })
            })
            .collect();
        Some(ApplicationControllerStatusSection {
            deployments,
            cleanup_evidence: evidence
                .iter()
                .map(|evidence| {
                    application_cleanup_evidence(
                        &evidence.application_deployment_id,
                        evidence.lifecycle_generation,
                        evidence.realization_generation,
                    )
                })
                .collect(),
        })
    }

    fn accept_receipts(&self, acknowledged: &BTreeSet<(String, u64)>) -> Result<(), StoreError> {
        self.state
            .acknowledge_node_application_cleanup_evidence(acknowledged)
    }

    fn retire_withdrawn_cleanup_evidence(&self) {
        let installed = self
            .input
            .installed()
            .map(|installed| {
                installed
                    .assignments
                    .iter()
                    .filter_map(|assignment| assignment.declaration.as_ref())
                    .filter(|declaration| {
                        declaration.desired_state
                            == ApplicationDeploymentDesiredState::Teardown as i32
                    })
                    .map(|declaration| {
                        (
                            declaration.application_deployment_id.clone(),
                            declaration.lifecycle_generation,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Err(error) = self
            .state
            .retire_acknowledged_application_cleanup_evidence(&installed)
        {
            warn!(event_kind = "node_application_cleanup_retirement", outcome = "failed", error = %error, "Node could not retire acknowledged application cleanup evidence");
        }
    }
}

impl HostedApplicationResources for NodeApplicationStatus {
    /// Attributes every deployment the status reports, all from one collection: each unit's host
    /// CPU and memory beside its accelerator memory exactly as the status reports it.
    fn hosted_application_resources(&self) -> Vec<HostedApplication> {
        let collection = self.resources.collection();
        let Some(section) = self.status_section(&collection) else {
            return Vec::new();
        };
        section
            .deployments
            .into_iter()
            .map(|status| {
                let host = status
                    .placement
                    .as_ref()
                    .and_then(|placement| SiteNodeId::parse(&placement.site_node_id).ok())
                    .map(|node| {
                        self.resources.projection(
                            &status.application_deployment_id,
                            &node,
                            &collection,
                        )
                    })
                    .unwrap_or_default();
                let mut accelerator = status
                    .service_accelerator_memory
                    .into_iter()
                    .map(|unit| (unit.service_name, unit.accelerator_memory))
                    .collect::<BTreeMap<_, _>>();
                let names = host
                    .keys()
                    .chain(accelerator.keys())
                    .cloned()
                    .collect::<BTreeSet<_>>();
                HostedApplication {
                    deployment_id: status.application_deployment_id,
                    services: names
                        .into_iter()
                        .map(|service_name| HostedService {
                            host: host.get(&service_name).copied(),
                            accelerator_memory: accelerator.remove(&service_name).flatten(),
                            service_name,
                        })
                        .collect(),
                }
            })
            .collect()
    }
}

impl NodeApplicationStatus {
    /// Reports one assignment from the node record that reconciled its exact lifecycle
    /// generation; without one it reports Reconciling, so an earlier generation's observation
    /// never stands in for the assigned one. Pins are reported until the manager projects them,
    /// and accelerator memory is attributed from `collection`.
    fn deployment_status(
        &self,
        installed: &InstalledApplicationSection,
        assignment: &ApplicationAssignment,
        declaration: &ApplicationDeploymentDeclaration,
        record: Option<&CurrentApplicationDeployment>,
        realization: Option<&NodeApplicationRealization>,
        collection: &HardwareCollection,
    ) -> ApplicationDeploymentStatus {
        let mut status = ApplicationDeploymentStatus {
            application_deployment_id: declaration.application_deployment_id.clone(),
            lifecycle_generation: declaration.lifecycle_generation,
            placement: declaration.placement.clone(),
            realization_generation: None,
            accepted_view_revision: 0,
            state_revision: record.map_or(0, |record| record.record_revision),
            lifecycle: ApplicationLifecycleObservation::Reconciling as i32,
            stage: ApplicationStatusStage::Unspecified as i32,
            readiness: dotcore_proto::dotcore::application::v1::ApplicationReadinessState::NotReady
                as i32,
            failure_code: ApplicationStatusFailureCode::Unspecified as i32,
            services: Vec::new(),
            endpoint: None,
            service_accelerator_memory: Vec::new(),
            steps: None,
            resolved_launch_identities: realization
                .map(|realization| {
                    realization
                        .resolved_launch_identities
                        .iter()
                        .filter(|pin| {
                            !declaration
                                .node_resolved_launch_identities
                                .iter()
                                .any(|projected| projected.selection_key == pin.selection_key)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        };
        if let Err(failure_code) = classify_application_assignment(
            &installed.node_id,
            &installed.execution_targets,
            assignment,
        ) {
            status.lifecycle = ApplicationLifecycleObservation::Unavailable as i32;
            status.failure_code = failure_code as i32;
            return status;
        }
        let Some((record, state)) = record
            .filter(|record| {
                record.reconciled_lifecycle_generation == Some(declaration.lifecycle_generation)
            })
            .and_then(|record| Some((record, record.state.as_ref()?)))
        else {
            return status;
        };
        let lifecycle = private_lifecycle(state.lifecycle_state);
        status.state_revision = record.record_revision;
        status.lifecycle = lifecycle as i32;
        status.stage = super::status_text::status_stage(&state.stage) as i32;
        status.readiness = state.readiness_state;
        status.failure_code = super::status_text::status_failure_code(state) as i32;
        status.services = state.services.clone();
        status.steps = state.steps.clone();
        status.service_accelerator_memory = service_accelerator_memory(
            &installed.node_id,
            self.accelerator_memory.projection(
                declaration,
                state.lifecycle_state(),
                &installed.execution_targets,
                collection,
            ),
        );
        status.endpoint = state.endpoint.as_ref().and_then(published_endpoint);
        status.realization_generation = realization
            .and_then(|realization| realization.realization_generation)
            .filter(|_| {
                !matches!(
                    lifecycle,
                    ApplicationLifecycleObservation::Queued
                        | ApplicationLifecycleObservation::Stopped
                )
            });
        status.accepted_view_revision = self
            .reconciled
            .revision(
                &declaration.application_deployment_id,
                &ActionAssignment::from(assignment),
                &installed.manager_session,
            )
            .unwrap_or(0);
        status
    }
}

/// Classifies one installed application assignment for the node that hosts it. A problem is
/// that deployment's typed failure, never a rejection of the view: an assignment that does not
/// validate as a complete declaration is invalid, except that a local-development selection may
/// leave its launch identity for this node to resolve once; one whose exact placement does not name an
/// application target of this view on `node_id` with the snapshot's platform is a target
/// mismatch, and a linked route the route manager does not know is reported as such.
pub(crate) fn classify_application_assignment(
    node_id: &str,
    execution_targets: &[ExecutionTarget],
    assignment: &ApplicationAssignment,
) -> Result<(), ApplicationStatusFailureCode> {
    let declaration = assignment
        .declaration
        .as_ref()
        .ok_or(ApplicationStatusFailureCode::InvalidAssignment)?;
    crate::application::repository::validate_declaration(declaration)
        .map_err(|_| ApplicationStatusFailureCode::InvalidAssignment)?;
    let placement = declaration
        .placement
        .as_ref()
        .ok_or(ApplicationStatusFailureCode::InvalidAssignment)?;
    let snapshot = declaration
        .snapshot
        .as_ref()
        .ok_or(ApplicationStatusFailureCode::InvalidAssignment)?;
    let target = execution_targets
        .iter()
        .find(|target| target.execution_target_id == placement.execution_target_id)
        .filter(|target| {
            placement.site_node_id == node_id
                && target.site_node_id == node_id
                && matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                )
        })
        .ok_or(ApplicationStatusFailureCode::TargetMismatch)?;
    if snapshot.placement.as_ref() != Some(placement)
        || target.platform.as_ref() != snapshot.platform.as_ref()
    {
        return Err(ApplicationStatusFailureCode::TargetMismatch);
    }
    let resolution = assignment
        .linked_route
        .as_ref()
        .map(|linked| linked.resolution.as_ref());
    match (declaration.inference_route_id.as_ref(), resolution) {
        (None, None) => Ok(()),
        (
            Some(_),
            Some(Some(application_linked_route_projection::Resolution::RouteName(route_name))),
        ) if is_valid_route_name(route_name) => Ok(()),
        (Some(_), Some(Some(application_linked_route_projection::Resolution::Unknown(_)))) => {
            Err(ApplicationStatusFailureCode::LinkedRouteUnknown)
        }
        _ => Err(ApplicationStatusFailureCode::InvalidAssignment),
    }
}

/// Maps a recorded lifecycle to the private observation; an unspecified one still reconciles.
fn private_lifecycle(lifecycle: i32) -> ApplicationLifecycleObservation {
    match ApplicationLifecycleState::try_from(lifecycle) {
        Ok(ApplicationLifecycleState::Queued) => ApplicationLifecycleObservation::Queued,
        Ok(ApplicationLifecycleState::Preparing) => ApplicationLifecycleObservation::Preparing,
        Ok(ApplicationLifecycleState::Starting | ApplicationLifecycleState::Running) => {
            ApplicationLifecycleObservation::Starting
        }
        Ok(ApplicationLifecycleState::Ready) => ApplicationLifecycleObservation::Ready,
        Ok(ApplicationLifecycleState::Pausing) => ApplicationLifecycleObservation::Pausing,
        Ok(ApplicationLifecycleState::Paused) => ApplicationLifecycleObservation::Paused,
        Ok(ApplicationLifecycleState::Stopping) => ApplicationLifecycleObservation::Stopping,
        Ok(ApplicationLifecycleState::Stopped) => ApplicationLifecycleObservation::Stopped,
        Ok(ApplicationLifecycleState::Failed | ApplicationLifecycleState::Unavailable) => {
            ApplicationLifecycleObservation::Unavailable
        }
        Ok(ApplicationLifecycleState::Unspecified) | Err(_) => {
            ApplicationLifecycleObservation::Reconciling
        }
    }
}

/// Extracts the published host and port from a recorded `http://host:port` endpoint.
fn published_endpoint(
    endpoint: &dotcore_proto::dotcore::application::v1::ApplicationEndpoint,
) -> Option<ApplicationPublishedEndpoint> {
    let address = endpoint
        .url
        .strip_prefix("http://")?
        .parse::<SocketAddrV4>()
        .ok()?;
    Some(ApplicationPublishedEndpoint {
        host: address.ip().to_string(),
        port: u32::from(address.port()),
    })
}

/// Lists each unit's projected accelerator memory, bounded to what the manager accepts from
/// `node_id`; a unit the projection does not cover is not listed, never reported as a false zero.
fn service_accelerator_memory(
    node_id: &str,
    memory: BTreeMap<String, AcceleratorMemoryObservation>,
) -> Vec<ApplicationServiceAcceleratorMemory> {
    memory
        .into_iter()
        .map(
            |(service_name, memory)| ApplicationServiceAcceleratorMemory {
                service_name,
                accelerator_memory: Some(bounded_accelerator_memory(node_id, memory)),
            },
        )
        .collect()
}

fn node_records_error(error: impl Into<CurrentStateError>) -> ApplicationLifecycleError {
    match error.into() {
        error @ (CurrentStateError::OutcomeUncertain(_) | CurrentStateError::Poisoned) => {
            outcome_uncertain_with_source(
                "Application node record persistence outcome is uncertain",
                error,
            )
        }
        error => super::configuration_with_source("Application node records failed", error),
    }
}

/// Reports a node-local read that prevented building application status; the next status
/// report tries again.
fn warn_status_not_built(source: &'static str, error: &impl std::fmt::Display) {
    warn!(event_kind = "node_application_status", outcome = "not_built", source, error = %error, "Node could not build application status; it will retry with the next report");
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        net::Ipv4Addr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use dotcore_artifact::{
        ArtifactManager, LocalImageIdentityResolutionError, LocalImageIdentityResolutionFuture,
        LocalImageIdentityResolver, OciImagePreparer, OciPreparationError, OciPreparationFuture,
        OciPreparationRequest, PreparationStep,
    };
    use dotcore_domain::{
        ExecutionPlatform, SiteNodeId,
        clock::SystemClock,
        hardware_observation::{
            AcceleratorProcessObservation, AcceleratorVendorProcessObservation,
            AcceleratorVendorProcessResult, NodeHardwareObservation, ObservedAcceleratorProcess,
        },
        host_process::HostProcessIdentity,
        test_support::oci::ServiceArtifactOci as _,
        timing::Timing,
    };
    use dotcore_proto::dotcore::{
        application::{
            private::v1::{
                ApplicationAssignment, ApplicationControllerStatusSection,
                ApplicationDeploymentStatus, ApplicationLifecycleObservation,
                ApplicationLinkedRouteProjection, ApplicationPublishedEndpoint,
                ApplicationStatusFailureCode, ApplicationStatusStage,
                application_linked_route_projection,
            },
            v1::{
                ApplicationConfigurationAttachment, ApplicationDeploymentDeclaration,
                ApplicationDeploymentDesiredState, ApplicationLifecycleState,
                ApplicationReadinessState, ApplicationSecretAttachment,
                ApplicationServiceAcceleratorMemory, ApplicationStorageAttachment,
                CurrentApplicationDeployment, ResolvedApplicationDeploymentSnapshot,
                ResolvedApplicationServiceSelection,
            },
        },
        catalog::v1::{
            ApplicationHttpReadiness, ApplicationServiceReadiness, ParameterValue, StorageBinding,
            StorageKind, StorageScope, application_service_readiness, parameter_value,
        },
        deployment::private::v1::{
            DeploymentLogSourceOpening, DeploymentLogTarget, DeploymentLogUnavailableReason,
            DeploymentOwnerKind, FollowDeploymentLogsRequest, FollowDeploymentLogsResponse,
            follow_deployment_logs_response,
        },
        deployment::v1::{
            ArtifactPreparationPolicy, ExactPlacement, NodeResolvedLaunchIdentity,
            {
                AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation,
                AcceleratorMemoryRefreshState, AcceleratorMemoryUnavailable,
                AcceleratorMemoryUsageSet, accelerator_memory_observation,
                accelerator_memory_refresh_state,
            },
        },
        observation::v1::{AcceleratorVendor, HardwareSnapshot},
        site::v1::{
            ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget,
            application_runtime_binding, execution_target,
        },
    };
    use dotcore_runtime::{
        ObservedWorkloadGroup, RuntimeBackendError, RuntimeBackendId, RuntimeFuture,
        RuntimeGroupLogFollowQuery, RuntimeLogEntry, RuntimeLogEvent, RuntimeLogFuture,
        RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceHistory, RuntimeLogSourceSelection,
        RuntimeLogSourceStream, RuntimeLogStream, RuntimeLogStreamKind, WorkloadCorrelation,
        WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupLaunch, WorkloadGroupObservation,
        WorkloadStorageAttachment, WorkloadStorageKey, WorkloadStorageKind, WorkloadUnitName,
        WorkloadUnitObservation, WorkloadUnitState,
    };
    use dotcore_runtime_backends::simulated::{
        InMemoryRuntimeBackend, SimulatedRuntimeConfig, SimulatedRuntimeGroupCall,
        SimulatedRuntimeTestSupport,
    };
    use dotcore_site::{
        node::hardware_inventory::{
            HardwareCollection, HardwareInventoryEntry, HardwareInventoryPublisher,
            HardwareInventorySource,
        },
        store::FileNodeState,
        view::{
            contract::application_cleanup_evidence,
            fencing::{
                AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
            },
        },
    };
    use futures_util::StreamExt as _;

    use super::super::accelerator_memory::GroupAcceleratorAttribution;
    use super::{NodeApplicationComposition, NodeApplicationRealizer, service_accelerator_memory};
    use crate::{
        application::{
            controller::test_support::{
                application_execution_target, application_with_route_input, deployment,
                resolved_snapshot, secret_parameter, start_steps,
            },
            repository::CurrentApplicationRepository as _,
        },
        deployment_logs::{
            DeploymentLogOwners, DeploymentLogReader, OwnerLogSources, PreparationOwner,
            service_steps,
        },
        dispatch::application::ApplicationRuntimeMapBuilder,
        management_logs::{
            DeploymentLogError, DeploymentLogFollowQuery, DeploymentLogReadQuery,
            ManagementLogAdmission, ProtectedLogValues, filter_entry,
        },
    };

    const LEASE: Duration = Duration::from_secs(30);

    /// Bound on waiting for one follow frame, far above its expected delay.
    const PROMPT: Duration = Duration::from_secs(5);
    const DEV_TAG: &str = "example.test/open-webui:dev";
    const FIRST_IMAGE: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const REBUILT_IMAGE: &str =
        "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const PASSWORD_HASH: &str = "$2b$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";

    #[tokio::test]
    async fn node_realizes_its_valid_assignments_and_reports_each_typed_status() {
        let endpoint = ReadyEndpoint::start().await;
        let fixture = NodeFixture::new(Some(endpoint.port));
        let ready = assignment(declaration("ready-1", 1, Present), None);
        let mut mismatched = declaration("mismatched-1", 1, Present);
        mismatched.placement.as_mut().unwrap().execution_target_id = "model-target-one".to_owned();
        fixture.install(1, vec![ready.clone(), assignment(mismatched, None)]);

        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("reconcile pass");

        assert_eq!(
            fixture.support.group_calls(),
            [SimulatedRuntimeGroupCall::create("ready-1")],
            "only the valid assignment reaches the node runtime"
        );
        let launch_generation = fixture.launch_generation("ready-1");
        let section = status_of(&node);
        let status = deployment_status(&section, "ready-1");
        assert_eq!(
            status.lifecycle,
            ApplicationLifecycleObservation::Ready as i32
        );
        assert_eq!(status.stage, ApplicationStatusStage::Ready as i32);
        assert_eq!(status.readiness, ApplicationReadinessState::Ready as i32);
        assert_eq!(status.lifecycle_generation, 1);
        assert_eq!(status.accepted_view_revision, 1);
        assert_eq!(status.realization_generation, Some(launch_generation));
        assert_ne!(status.state_revision, 0);
        assert_eq!(
            status.endpoint,
            Some(ApplicationPublishedEndpoint {
                host: "127.0.0.1".to_owned(),
                port: u32::from(endpoint.port),
            })
        );
        assert_eq!(
            status.failure_code,
            ApplicationStatusFailureCode::Unspecified as i32
        );
        let mismatched = deployment_status(&section, "mismatched-1");
        assert_eq!(
            (
                mismatched.lifecycle,
                mismatched.failure_code,
                mismatched.state_revision
            ),
            (
                ApplicationLifecycleObservation::Unavailable as i32,
                ApplicationStatusFailureCode::TargetMismatch as i32,
                0
            )
        );

        // An unrelated installed revision keeps the reconciled view until the next pass.
        fixture.install(2, vec![ready]);
        assert_eq!(
            deployment_status(&status_of(&node), "ready-1").accepted_view_revision,
            1
        );
        node.reconcile_for_test().await.expect("second pass");
        assert_eq!(
            deployment_status(&status_of(&node), "ready-1").accepted_view_revision,
            2
        );
        assert_eq!(
            fixture.support.group_calls().len(),
            1,
            "a ready group is left as is"
        );
    }

    #[tokio::test]
    async fn status_reports_the_units_accelerator_memory_projected_by_the_node() {
        let endpoint = ReadyEndpoint::start().await;
        let fixture = NodeFixture::new(Some(endpoint.port));
        let ready = assignment(declaration("ready-1", 1, Present), None);
        fixture.install(1, vec![ready]);

        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("reconcile pass");

        let status = deployment_status(&status_of(&node), "ready-1");
        assert_eq!(
            status.service_accelerator_memory,
            [ApplicationServiceAcceleratorMemory {
                service_name: "web".to_owned(),
                accelerator_memory: Some(memory_not_applicable()),
            }],
            "a simulated target reads not applicable, never a false zero"
        );
    }

    #[tokio::test]
    async fn a_retained_deployment_without_an_observed_group_forgets_its_unit_identities() {
        let endpoint = ReadyEndpoint::start().await;
        let fixture = NodeFixture::new(Some(endpoint.port));
        let mut declaration = declaration("memory-1", 1, Present);
        fixture.install(1, vec![assignment(declaration.clone(), None)]);
        let node = fixture.realizer();
        let attribution = &node.status.accelerator_memory;
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_memory(
            &publisher,
            AcceleratorVendor::Nvidia,
            vec![memory_process("GPU-a", 41, 700, 1000)],
        );
        attribution.observe(
            "memory-1",
            &memory_group(&[("web", Some(memory_identity("node-a", 41, 700)))]),
        );
        // Read through a dedicated-memory target, so a retained identity would still attribute.
        declaration.placement = memory_deployment(Vec::new()).placement;
        declaration
            .snapshot
            .as_mut()
            .expect("snapshot")
            .accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
        let targets = [memory_target(vec!["nvidia.com/gpu=0".to_owned()])];
        let web = |attribution: &GroupAcceleratorAttribution| {
            attribution
                .projection(
                    &declaration,
                    ApplicationLifecycleState::Ready,
                    &targets,
                    &inventory.collection(),
                )
                .remove("web")
                .expect("web unit")
        };
        assert_eq!(web(attribution), memory_available(1000));

        // The pass discovers no group for the deployment before it creates one.
        node.reconcile_for_test().await.expect("reconcile pass");

        assert_eq!(
            web(attribution),
            memory_unavailable(
                "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
                "Runtime did not report an authoritative workload process identity.",
                Some(memory_refresh()),
            )
        );
    }

    #[test]
    fn unit_memory_is_listed_per_target_shape_and_bounded_to_the_reporting_node() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let attribution = GroupAcceleratorAttribution::default();
        let node_id = SiteNodeId::for_test("node-a").to_string();
        let declaration = memory_deployment(vec!["nvidia.com/gpu=0".to_owned()]);
        let observe = |units: &[(&str, Option<HostProcessIdentity>)]| {
            attribution.observe(&declaration.application_deployment_id, &memory_group(units));
        };
        let listed = |memory: BTreeMap<String, AcceleratorMemoryObservation>| {
            service_accelerator_memory(&node_id, memory)
                .into_iter()
                .map(|unit| (unit.service_name, unit.accelerator_memory))
                .collect::<Vec<_>>()
        };

        // A dedicated-memory target attributes each unit's observed process rows to it.
        publish_memory(
            &publisher,
            AcceleratorVendor::Nvidia,
            vec![
                memory_process("GPU-a", 41, 700, 1000),
                memory_process("GPU-a", 42, 701, 2000),
            ],
        );
        observe(&[
            ("cache", Some(memory_identity("node-a", 42, 701))),
            ("web", Some(memory_identity("node-a", 41, 700))),
        ]);
        let memory = attribution.projection(
            &declaration,
            ApplicationLifecycleState::Ready,
            &[memory_target(vec!["nvidia.com/gpu=0".to_owned()])],
            &inventory.collection(),
        );
        assert_eq!(
            listed(memory),
            [
                ("cache".to_owned(), Some(memory_available(2000))),
                ("web".to_owned(), Some(memory_available(1000))),
            ]
        );

        // A target with no per-process rows reads explicit unavailable, never a zero.
        publish_memory(&publisher, AcceleratorVendor::Apple, Vec::new());
        let memory = attribution.projection(
            &memory_deployment(vec!["apple-0".to_owned()]),
            ApplicationLifecycleState::Ready,
            &[memory_target(vec!["apple-0".to_owned()])],
            &inventory.collection(),
        );
        let unavailable = memory_unavailable(
            "PROCESS_NOT_OBSERVED",
            "An authoritative workload process was not observed on an accelerator.",
            Some(memory_refresh()),
        );
        assert_eq!(
            listed(memory),
            [
                ("cache".to_owned(), Some(unavailable.clone())),
                ("web".to_owned(), Some(unavailable)),
            ]
        );

        // A deployment that retained no accelerator device has no accelerator workload.
        let memory = attribution.projection(
            &memory_deployment(Vec::new()),
            ApplicationLifecycleState::Ready,
            &[memory_target(Vec::new())],
            &inventory.collection(),
        );
        assert_eq!(
            listed(memory),
            [
                ("cache".to_owned(), Some(memory_not_applicable())),
                ("web".to_owned(), Some(memory_not_applicable())),
            ]
        );

        // Usage on another node is dropped before the manager would reject the status.
        let mut elsewhere = memory_available(1000);
        if let Some(accelerator_memory_observation::Result::Available(usage)) =
            elsewhere.result.as_mut()
        {
            usage.devices[0].managed_node_id = "node-b".to_owned();
        }
        let mut bounded = elsewhere.clone();
        bounded.result = Some(accelerator_memory_observation::Result::Available(
            AcceleratorMemoryUsageSet::default(),
        ));
        assert_eq!(
            listed(BTreeMap::from([("web".to_owned(), elsewhere)])),
            [("web".to_owned(), Some(bounded))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_view_lease_freezes_actions_and_leaves_groups_running() {
        let fixture = NodeFixture::new(None);
        fixture.install(1, vec![assignment(declaration("app-1", 1, Present), None)]);
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("first pass");
        let before = fixture.current("app-1");

        fixture.install(2, vec![assignment(declaration("app-1", 2, Paused), None)]);
        tokio::time::advance(LEASE + Duration::from_secs(1)).await;
        node.reconcile_for_test()
            .await
            .expect("frozen pass never fails");

        assert_eq!(
            fixture.support.group_calls(),
            [SimulatedRuntimeGroupCall::create("app-1")],
            "no stop starts after the lease expired"
        );
        assert_eq!(
            fixture.current("app-1"),
            before,
            "the frozen deployment keeps its state"
        );
        assert_eq!(fixture.group_running("app-1").await, Some(true));
        let frozen = deployment_status(&status_of(&node), "app-1");
        assert_eq!(
            frozen.lifecycle,
            ApplicationLifecycleObservation::Reconciling as i32,
            "generation 1 state never stands in for paused generation 2"
        );

        fixture.install(2, vec![assignment(declaration("app-1", 2, Paused), None)]);
        node.reconcile_for_test().await.expect("renewed pass");
        assert_eq!(
            fixture.support.group_calls(),
            [
                SimulatedRuntimeGroupCall::create("app-1"),
                SimulatedRuntimeGroupCall::stop("app-1"),
            ]
        );
    }

    #[tokio::test]
    async fn local_development_pin_is_resolved_once_and_reused_through_pause_resume_and_restart() {
        let images = FakeImageStore::new([(DEV_TAG, FIRST_IMAGE)]);
        let mut fixture = NodeFixture::new(None).with_images(images.clone());
        fixture.install(
            1,
            vec![assignment(
                with_deployment_data(local_declaration("dev-1", 1, Present)),
                None,
            )],
        );
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("create pass");

        assert_eq!(images.resolutions(), 1);
        assert_eq!(fixture.launched_images("dev-1"), [FIRST_IMAGE]);
        assert_eq!(fixture.pins("dev-1"), [pin("web", FIRST_IMAGE)]);
        assert_eq!(
            deployment_status(&status_of(&node), "dev-1").resolved_launch_identities,
            [pin("web", FIRST_IMAGE)]
        );
        assert!(fixture.support.storage_exists(&deployment_data("dev-1")));

        // A rebuilt tag never replaces the pin across pause, resume, and node restart.
        images.retag(DEV_TAG, REBUILT_IMAGE);
        fixture.install(
            2,
            vec![assignment(
                with_deployment_data(local_declaration("dev-1", 2, Paused)),
                None,
            )],
        );
        node.reconcile_for_test().await.expect("pause pass");
        fixture.install(
            3,
            vec![assignment(
                with_deployment_data(local_declaration("dev-1", 3, Present)),
                None,
            )],
        );
        node.reconcile_for_test().await.expect("resume pass");
        drop(node);
        fixture.reopen_state();
        let restarted = fixture.realizer();
        restarted
            .reconcile_for_test()
            .await
            .expect("restarted pass");

        assert_eq!(
            images.resolutions(),
            1,
            "a pinned selection is never re-resolved"
        );
        assert_eq!(
            fixture.support.group_calls(),
            [
                SimulatedRuntimeGroupCall::create("dev-1"),
                SimulatedRuntimeGroupCall::stop("dev-1"),
                SimulatedRuntimeGroupCall::start("dev-1"),
            ],
            "restart recovery observes the retained group without recreating it"
        );
        assert_eq!(fixture.launched_images("dev-1"), [FIRST_IMAGE]);
        assert_eq!(fixture.pins("dev-1"), [pin("web", FIRST_IMAGE)]);
        assert!(
            fixture.support.storage_exists(&deployment_data("dev-1")),
            "storage survives"
        );

        // Once the manager projects the pin back, it is no longer reported.
        let mut projected = with_deployment_data(local_declaration("dev-1", 3, Present));
        projected.node_resolved_launch_identities = vec![pin("web", FIRST_IMAGE)];
        fixture.install(4, vec![assignment(projected, None)]);
        restarted
            .reconcile_for_test()
            .await
            .expect("projected pass");
        assert!(
            deployment_status(&status_of(&restarted), "dev-1")
                .resolved_launch_identities
                .is_empty()
        );
        assert_eq!(images.resolutions(), 1);
    }

    #[tokio::test]
    async fn preparation_writes_each_service_step_once_and_appends_a_retried_attempt() {
        const WEB: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        const WORKER: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-docker-image-worker";
        let images = crate::test_support::canonical_tempdir().expect("image fixture");
        let fixture_images = dotcore_artifact::DockerImageFixture::new(images.path());
        for store_path in [WEB, WORKER] {
            fixture_images.add_image(store_path, "amd64");
        }
        // The web service is not built yet: its first build fails, the next one adds it to the
        // store and roots it.
        let web_entry = WEB.trim_start_matches("/nix/store/");
        std::fs::remove_file(fixture_images.roots().join(web_entry)).expect("unbuilt web service");
        let web_package = fixture_images.package(WEB);
        let unbuilt = images.path().join("unbuilt-web");
        std::fs::rename(&web_package, &unbuilt).expect("web service out of the store");
        let fail_once = images.path().join("fail-once");
        std::fs::write(&fail_once, "").expect("first build failure");
        let nix = fixture_images.nix_directory().join("nix");
        std::fs::write(
            &nix,
            format!(
                "#!/bin/sh\n\
                 case \" $* \" in *\" build \"*) ;; *) exit 1 ;; esac\n\
                 out=; previous=\n\
                 for argument in \"$@\"; do\n\
                 [ \"$previous\" = --out-link ] && out=$argument\n\
                 previous=$argument\n\
                 done\n\
                 echo 'building web' >&2\n\
                 if [ -e '{fail_once}' ]; then rm '{fail_once}'; echo 'error: builder failed' >&2; exit 1; fi\n\
                 mv '{unbuilt}' '{web_package}'\n\
                 ln -s '{WEB}' \"$out\"\n\
                 echo '{WEB}'\n",
                fail_once = fail_once.display(),
                unbuilt = unbuilt.display(),
                web_package = web_package.display(),
            ),
        )
        .expect("scripted nix");
        let fixture = NodeFixture::new(None).with_artifacts(fixture_images.manager("x86_64-linux"));
        let mut deployment = docker_delivered_declaration(
            "app-1",
            1,
            Present,
            &dotcore_domain::test_support::nix::locked_flake(),
            &[("x86_64-linux", WEB)],
        );
        let snapshot = deployment.snapshot.as_mut().unwrap();
        let graph = snapshot.graph.as_mut().unwrap();
        let mut worker = graph.services[0].clone();
        worker.service_name = "worker".to_owned();
        worker.ports.clear();
        worker.readiness = None;
        match worker.artifact.as_mut().unwrap().kind.as_mut().unwrap() {
            dotcore_proto::dotcore::catalog::v1::application_service_artifact::Kind::Nix(nix) => {
                nix.package = Some("apps.worker.docker".to_owned());
                nix.store_paths[0].store_path = WORKER.to_owned();
            }
            _ => unreachable!("a Docker-delivered Nix service"),
        }
        graph.services.push(worker);
        let mut selection = snapshot.service_selections[0].clone();
        selection.service_name = "worker".to_owned();
        snapshot.service_selections.push(selection);
        fixture.install(1, vec![assignment(deployment, None)]);
        let node = fixture.realizer();
        let preparation = fixture.preparation_logs();
        let log = |step| {
            let preparation = preparation.clone();
            async move {
                preparation
                    .read(
                        "app-1",
                        &[step],
                        &RuntimeLogQuery::new(std::num::NonZeroU16::new(200).expect("limit"), None)
                            .expect("query"),
                    )
                    .await
                    .expect("preparation log")
                    .remove(0)
                    .result
                    .expect("readable preparation log")
                    .entries
                    .into_iter()
                    .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
                    .collect::<Vec<_>>()
            }
        };

        node.reconcile_for_test().await.expect("failed build pass");
        node.reconcile_for_test().await.expect("retried build pass");
        let logs = [
            log(PreparationStep::ServiceBuild {
                service: "web".to_owned(),
            })
            .await,
            log(PreparationStep::ServiceImage {
                service: "web".to_owned(),
            })
            .await,
            log(PreparationStep::ServiceBuild {
                service: "worker".to_owned(),
            })
            .await,
            log(PreparationStep::ServiceImage {
                service: "worker".to_owned(),
            })
            .await,
        ];
        node.reconcile_for_test().await.expect("warm pass");
        // A restarted node process prepares again with fresh realizer and log state.
        fixture
            .realizer()
            .reconcile_for_test()
            .await
            .expect("restarted warm pass");

        // Durations vary, and the loaders print their result record.
        let ended = |lines: &[String]| {
            lines
                .iter()
                .map(|line| {
                    if line.starts_with("Finished in ") {
                        "Finished".to_owned()
                    } else if line.starts_with("{\"identityVerified\":true") {
                        "loader result".to_owned()
                    } else {
                        line.clone()
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ended(&logs[0]),
            [
                "Preparing",
                "building web",
                "error: builder failed",
                "Failed: nix build of \"github:dotcoreai/dotcore/0123456789abcdef0123456789abcdef01234567\
                 #legacyPackages.x86_64-linux.apps.web.docker\" failed on this node; check the Nix \
                 build log on this node, then retry: building web; error: builder failed",
                "Preparing again in a new attempt",
                "building web",
                "Finished",
            ]
        );
        assert_eq!(ended(&logs[1]), ["Preparing", "loader result", "Finished"]);
        assert_eq!(
            logs[2],
            [format!("Already built: {WORKER}")],
            "the worker service was already built"
        );
        assert_eq!(ended(&logs[3]), ["Preparing", "loader result", "Finished"]);
        let warm = [
            log(PreparationStep::ServiceBuild {
                service: "web".to_owned(),
            })
            .await,
            log(PreparationStep::ServiceImage {
                service: "web".to_owned(),
            })
            .await,
            log(PreparationStep::ServiceBuild {
                service: "worker".to_owned(),
            })
            .await,
            log(PreparationStep::ServiceImage {
                service: "worker".to_owned(),
            })
            .await,
        ];
        assert_eq!(
            warm, logs,
            "warm passes, also after a restart, write nothing more"
        );
        assert_eq!(fixture_images.loader_runs(WEB), 1);
    }

    #[tokio::test]
    async fn docker_delivered_services_pin_the_loader_of_the_node_system_once() {
        const LINUX: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        const EVALUATED: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        let images = crate::test_support::canonical_tempdir().expect("image fixture");
        let fixture_images = dotcore_artifact::DockerImageFixture::new(images.path());
        for store_path in [LINUX, EVALUATED] {
            fixture_images.add_image(store_path, "amd64");
        }
        let (development_source, evaluator) =
            dotcore_artifact::FixedNixEvaluator::development_source(EVALUATED);
        let artifacts = fixture_images
            .manager("x86_64-linux")
            .with_nix_sources(development_source);
        let mut fixture = NodeFixture::new(None).with_artifacts(artifacts);
        let locked = |generation, state| {
            docker_delivered_declaration(
                "locked-1",
                generation,
                state,
                &dotcore_domain::test_support::nix::locked_flake(),
                &[("x86_64-linux", LINUX)],
            )
        };
        let development = |generation, state| {
            docker_delivered_declaration("dev-1", generation, state, "self", &[])
        };
        fixture.install(
            1,
            vec![
                assignment(locked(1, Present), None),
                assignment(development(1, Present), None),
            ],
        );
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("create pass");

        assert_eq!(fixture.pins("locked-1"), [pin("web", LINUX)]);
        assert_eq!(fixture.pins("dev-1"), [pin("web", EVALUATED)]);
        assert_eq!(
            fixture.launched_images("locked-1"),
            [dotcore_domain::nix::docker_launch_reference(LINUX).expect("reference")]
        );
        assert_eq!(
            fixture.launched_images("dev-1"),
            [dotcore_domain::nix::docker_launch_reference(EVALUATED).expect("reference")]
        );
        assert_eq!(evaluator.evaluations(), 1);
        assert_eq!(fixture_images.loader_runs(LINUX), 1);

        fixture.install(
            2,
            vec![
                assignment(locked(2, Paused), None),
                assignment(development(2, Paused), None),
            ],
        );
        node.reconcile_for_test().await.expect("pause pass");
        fixture.install(
            3,
            vec![
                assignment(locked(3, Present), None),
                assignment(development(3, Present), None),
            ],
        );
        node.reconcile_for_test().await.expect("resume pass");
        drop(node);
        fixture.reopen_state();
        fixture
            .realizer()
            .reconcile_for_test()
            .await
            .expect("restarted pass");

        assert_eq!(
            evaluator.evaluations(),
            1,
            "a pinned selection is never re-evaluated"
        );
        assert_eq!(fixture.pins("locked-1"), [pin("web", LINUX)]);
        assert_eq!(fixture.pins("dev-1"), [pin("web", EVALUATED)]);
        assert_eq!(
            fixture.launched_images("dev-1"),
            [dotcore_domain::nix::docker_launch_reference(EVALUATED).expect("reference")]
        );
    }

    #[tokio::test]
    async fn a_simulated_target_pins_synthetic_nix_services_without_evaluating_them() {
        const LINUX: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        const EVALUATED: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        let (development_source, evaluator) =
            dotcore_artifact::FixedNixEvaluator::development_source(EVALUATED);
        let cache = crate::test_support::canonical_tempdir().expect("cache");
        let resolver = ArtifactManager::new(cache.path().to_path_buf(), None)
            .expect("artifact manager")
            .with_nix_system(Some("x86_64-linux"))
            .with_nix_sources(development_source);
        let fixture = NodeFixture::new(None)
            .with_launch_pins(Arc::new(resolver))
            .virtualized();
        fixture.install(
            1,
            vec![
                assignment(
                    docker_delivered_declaration(
                        "locked-1",
                        1,
                        Present,
                        &dotcore_domain::test_support::nix::locked_flake(),
                        &[("x86_64-linux", LINUX)],
                    ),
                    None,
                ),
                assignment(
                    docker_delivered_declaration("dev-1", 1, Present, "self", &[]),
                    None,
                ),
            ],
        );

        fixture
            .realizer()
            .reconcile_for_test()
            .await
            .expect("create pass");

        // A locked artifact pins its catalog path; a development source a synthetic one.
        assert_eq!(fixture.pins("locked-1"), [pin("web", LINUX)]);
        assert_eq!(
            fixture.launched_images("locked-1"),
            [dotcore_domain::nix::docker_launch_reference(LINUX).expect("reference")]
        );
        let pins = fixture.pins("dev-1");
        let [pinned] = pins.as_slice() else {
            panic!("one pin, got {pins:?}");
        };
        let reference = dotcore_domain::nix::docker_launch_reference(&pinned.launch_identity)
            .expect("a synthetic loader store path");
        assert_eq!(pinned.selection_key, "web");
        assert!(
            reference.starts_with("dotcore-nix/docker-image-simulated-"),
            "{reference}"
        );
        assert_eq!(fixture.launched_images("dev-1"), [reference]);
        assert_eq!(evaluator.evaluations(), 0);
    }

    #[tokio::test]
    async fn a_simulated_deployment_prepares_no_image_on_a_node_that_prepares_images() {
        const LINUX: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        let images = crate::test_support::canonical_tempdir().expect("image fixture");
        let fixture_images = dotcore_artifact::DockerImageFixture::new(images.path());
        fixture_images.add_image(LINUX, "amd64");
        let mut fixture = NodeFixture::new(None)
            .with_artifacts(fixture_images.manager("x86_64-linux"))
            .virtualized();
        let locked = |generation, state| {
            docker_delivered_declaration(
                "locked-1",
                generation,
                state,
                &dotcore_domain::test_support::nix::locked_flake(),
                &[("x86_64-linux", LINUX)],
            )
        };
        fixture.install(1, vec![assignment(locked(1, Present), None)]);
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("create pass");
        fixture.install(2, vec![assignment(locked(2, Paused), None)]);
        node.reconcile_for_test().await.expect("pause pass");
        fixture.install(3, vec![assignment(locked(3, Present), None)]);
        node.reconcile_for_test().await.expect("resume pass");
        drop(node);
        fixture.reopen_state();
        fixture
            .realizer()
            .reconcile_for_test()
            .await
            .expect("restarted pass");

        assert_eq!(fixture.pins("locked-1"), [pin("web", LINUX)]);
        assert_eq!(
            fixture.launched_images("locked-1"),
            [dotcore_domain::nix::docker_launch_reference(LINUX).expect("reference")]
        );
        assert_eq!(fixture_images.loader_runs(LINUX), 0);
    }

    #[tokio::test]
    async fn a_simulated_deployment_is_ready_at_its_published_port_without_a_probe() {
        // Nothing listens on the published port, so a probe would never succeed.
        let fixture = NodeFixture::new(None).virtualized();
        let declaration = declaration("ready-1", 1, Present);
        let published_port = declaration.published_port;
        fixture.install(1, vec![assignment(declaration, None)]);

        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("reconcile pass");

        let section = status_of(&node);
        let status = deployment_status(&section, "ready-1");
        assert_eq!(status.readiness, ApplicationReadinessState::Ready as i32);
        assert_eq!(
            status.endpoint,
            Some(ApplicationPublishedEndpoint {
                host: "127.0.0.1".to_owned(),
                port: published_port,
            })
        );
        assert_eq!(
            status.steps, None,
            "a ready deployment reports no start steps"
        );
    }

    #[tokio::test]
    async fn a_restarted_node_reports_its_recorded_preparing_for_the_view_it_prepares_under() {
        let preparer = Arc::new(GatedImagePreparer::default());
        let fixture = NodeFixture::new(None);
        let artifacts = ArtifactManager::new(
            fixture.directory.path().join("cache"),
            Some(Arc::clone(&preparer) as Arc<dyn OciImagePreparer>),
        )
        .expect("artifact manager");
        let fixture = fixture.with_artifacts(artifacts);
        // The node restarted while it prepared this start's image.
        fixture
            .state()
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: "app-1".to_owned(),
                    record_revision: 0,
                    state: Some(super::super::with_start_steps(
                        None,
                        super::super::preparing_state(None),
                    )),
                    reconciled_lifecycle_generation: Some(1),
                },
                None,
            )
            .expect("recorded preparing");
        let recorded = fixture.current("app-1").expect("recorded preparing");
        fixture.install(1, vec![assignment(declaration("app-1", 1, Present), None)]);
        let node = fixture.realizer();

        let (pass, (preparing, record)) = tokio::join!(node.reconcile_for_test(), async {
            preparer.started.notified().await;
            let status = deployment_status(&status_of(&node), "app-1");
            let record = fixture.current("app-1");
            preparer.release.notify_one();
            (status, record)
        });
        pass.expect("reconcile pass");

        assert_eq!(
            record,
            Some(recorded),
            "an unchanged preparing is not written again"
        );
        assert_eq!(
            preparing.lifecycle,
            ApplicationLifecycleObservation::Preparing as i32
        );
        assert_eq!(preparing.accepted_view_revision, 1);
        assert_eq!(preparing.steps, Some(start_steps(0)));
    }

    #[tokio::test]
    async fn status_reports_preparing_and_its_step_while_the_image_is_prepared() {
        let preparer = Arc::new(GatedImagePreparer::default());
        let fixture = NodeFixture::new(None);
        let artifacts = ArtifactManager::new(
            fixture.directory.path().join("cache"),
            Some(Arc::clone(&preparer) as Arc<dyn OciImagePreparer>),
        )
        .expect("artifact manager");
        let fixture = fixture.with_artifacts(artifacts);
        fixture.install(1, vec![assignment(declaration("app-1", 1, Present), None)]);
        let node = fixture.realizer();

        let (pass, preparing) = tokio::join!(node.reconcile_for_test(), async {
            preparer.started.notified().await;
            let status = deployment_status(&status_of(&node), "app-1");
            preparer.release.notify_one();
            status
        });
        pass.expect("reconcile pass");

        assert_eq!(
            preparing.lifecycle,
            ApplicationLifecycleObservation::Preparing as i32
        );
        assert_eq!(preparing.stage, ApplicationStatusStage::Preparing as i32);
        assert_eq!(preparing.accepted_view_revision, 1);
        assert_eq!(preparing.steps, Some(start_steps(0)));
        let started = deployment_status(&status_of(&node), "app-1");
        assert_eq!(
            started.lifecycle,
            ApplicationLifecycleObservation::Starting as i32
        );
        assert_eq!(
            started.stage,
            ApplicationStatusStage::EndpointNotReady as i32
        );
        assert_eq!(started.steps, Some(start_steps(2)));
    }

    #[tokio::test]
    async fn a_mac_pins_the_darwin_loader_of_a_linux_arm64_service() {
        use dotcore_domain::deployment::launch_pin::{LaunchPinResolver as _, PinSource};

        const LINUX: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        const DARWIN: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
        let declaration = docker_delivered_declaration(
            "locked-1",
            1,
            Present,
            &dotcore_domain::test_support::nix::locked_flake(),
            &[("aarch64-linux", LINUX), ("aarch64-darwin", DARWIN)],
        );
        let snapshot = declaration.snapshot.as_ref().expect("snapshot");
        let source = crate::application::repository::selection_pin_source(
            snapshot,
            &snapshot.service_selections[0],
        )
        .expect("pin source");
        assert!(matches!(source, PinSource::NixLocked { .. }));
        let cache = crate::test_support::canonical_tempdir().expect("cache");
        let manager = ArtifactManager::new(cache.path().to_path_buf(), None)
            .expect("artifact manager")
            .with_nix_system(Some("aarch64-darwin"));

        let pinned = manager
            .resolve_pin(
                &source,
                &dotcore_proto::dotcore::catalog::v1::OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: None,
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("pinned store path");

        assert_eq!(pinned, DARWIN);
    }

    #[tokio::test]
    async fn missing_pinned_image_and_unresolvable_tag_are_typed_failures() {
        let images = FakeImageStore::new([(DEV_TAG, FIRST_IMAGE)]);
        let mut fixture = NodeFixture::new(None).with_images(images.clone());
        fixture.install(
            1,
            vec![assignment(local_declaration("dev-1", 1, Present), None)],
        );
        fixture
            .realizer()
            .reconcile_for_test()
            .await
            .expect("create pass");

        // The node restarts on an empty runtime whose pinned image was deleted.
        images.remove_image(FIRST_IMAGE);
        images.retag(DEV_TAG, REBUILT_IMAGE);
        fixture.reopen_state();
        fixture.reset_runtime();
        let restarted = fixture.realizer();
        restarted
            .reconcile_for_test()
            .await
            .expect("failed deployment never fails the pass");

        let missing = deployment_status(&status_of(&restarted), "dev-1");
        assert_eq!(
            (missing.lifecycle, missing.failure_code),
            (
                ApplicationLifecycleObservation::Unavailable as i32,
                ApplicationStatusFailureCode::ImageMissing as i32
            )
        );
        assert_eq!(
            images.resolutions(),
            1,
            "a missing pinned image is never re-resolved"
        );
        assert!(fixture.support.group_calls().is_empty());

        let mut unresolved = local_declaration("dev-2", 1, Present);
        unresolved
            .snapshot
            .as_mut()
            .unwrap()
            .graph
            .as_mut()
            .unwrap()
            .services[0]
            .artifact
            .as_mut()
            .unwrap()
            .oci_mut()
            .unwrap()
            .tag = Some("missing".to_owned());
        fixture.install(2, vec![assignment(unresolved, None)]);
        restarted
            .reconcile_for_test()
            .await
            .expect("unresolved pass");
        let status = deployment_status(&status_of(&restarted), "dev-2");
        assert_eq!(
            status.failure_code,
            ApplicationStatusFailureCode::LaunchIdentityUnresolved as i32
        );
        assert!(fixture.pins("dev-2").is_empty());
        assert!(fixture.support.group_calls().is_empty());
    }

    #[tokio::test]
    async fn first_realization_fails_closed_on_unowned_application_data() {
        let fixture = NodeFixture::new(None);
        fixture.support.insert_storage(deployment_data("app-1"));
        fixture.install(1, vec![assignment(declaration("app-1", 1, Present), None)]);
        let node = fixture.realizer();

        for pass in 0..2 {
            node.reconcile_for_test().await.expect("fail-closed pass");
            let status = deployment_status(&status_of(&node), "app-1");
            assert_eq!(
                (status.lifecycle, status.failure_code),
                (
                    ApplicationLifecycleObservation::Unavailable as i32,
                    ApplicationStatusFailureCode::RecoveryRequired as i32
                ),
                "pass {pass}"
            );
        }
        assert!(fixture.support.group_calls().is_empty());
        assert!(
            fixture
                .state()
                .node_application_realization("app-1")
                .unwrap()
                .is_none()
        );
        assert!(
            fixture.support.storage_exists(&deployment_data("app-1")),
            "foreign data is untouched"
        );
    }

    #[tokio::test]
    async fn uncertain_start_of_a_newer_generation_fails_only_its_deployment() {
        for retained in [
            super::super::starting_state(None),
            super::super::preparing_state(None),
        ] {
            uncertain_start_fails_only_its_deployment(retained).await;
        }
    }

    async fn uncertain_start_fails_only_its_deployment(
        retained: dotcore_proto::dotcore::application::v1::ApplicationDeploymentState,
    ) {
        let fixture = NodeFixture::new(None);
        fixture
            .state()
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: "regressed-1".to_owned(),
                    record_revision: 0,
                    state: Some(retained),
                    reconciled_lifecycle_generation: Some(3),
                },
                None,
            )
            .expect("retained newer start");
        fixture.install(
            1,
            vec![
                assignment(declaration("regressed-1", 2, Present), None),
                assignment(declaration("healthy-1", 1, Present), None),
            ],
        );
        let node = fixture.realizer();
        node.reconcile_for_test()
            .await
            .expect("the pass never fails");

        assert_eq!(
            fixture.support.group_calls(),
            [SimulatedRuntimeGroupCall::create("healthy-1")]
        );
        let section = status_of(&node);
        let regressed = deployment_status(&section, "regressed-1");
        assert_eq!(
            (regressed.lifecycle, regressed.failure_code),
            (
                ApplicationLifecycleObservation::Unavailable as i32,
                ApplicationStatusFailureCode::RecoveryRequired as i32
            )
        );
        node.reconcile_for_test().await.expect("repeated pass");
        assert_eq!(
            fixture.support.group_calls().len(),
            1,
            "recovery is never retried silently"
        );
    }

    #[tokio::test]
    async fn teardown_removes_group_storage_and_preparation_logs_then_retains_evidence() {
        let fixture = NodeFixture::new(None);
        fixture.install(
            1,
            vec![assignment(
                with_deployment_data(declaration("app-1", 1, Present)),
                None,
            )],
        );
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("create pass");
        let attached = fixture
            .support
            .group_launches()
            .iter()
            .flat_map(|launch| launch.units.values())
            .flat_map(|unit| &unit.storage)
            .map(WorkloadStorageAttachment::storage_key)
            .collect::<Vec<_>>();
        assert_eq!(attached, [deployment_data("app-1")]);
        assert!(fixture.support.storage_exists(&deployment_data("app-1")));
        let launch_generation = fixture.launch_generation("app-1");
        let preparation = fixture.preparation_logs();
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for (deployment_id, step) in [
            (
                "app-1",
                PreparationStep::ServiceBuild {
                    service: "web".to_owned(),
                },
            ),
            (
                "app-1",
                PreparationStep::ServiceImage {
                    service: "web".to_owned(),
                },
            ),
            (
                "app-2",
                PreparationStep::ServiceImage {
                    service: "web".to_owned(),
                },
            ),
        ] {
            preparation.writer(deployment_id, step).append(
                time,
                RuntimeLogStreamKind::Stdout,
                b"prepared",
            );
        }
        let started = |deployment_id: &str| {
            fixture
                .preparation_logs()
                .select(
                    PreparationOwner::Application,
                    deployment_id,
                    &[],
                    &service_steps("web"),
                    1,
                )
                .steps
        };
        assert_eq!(started("app-1").len(), 2);

        let teardown = assignment(
            with_deployment_data(declaration("app-1", 2, Teardown)),
            None,
        );
        fixture.install(2, vec![teardown.clone()]);
        node.reconcile_for_test().await.expect("teardown pass");
        node.reconcile_for_test()
            .await
            .expect("repeated teardown pass");

        assert_eq!(
            fixture.support.group_calls(),
            [
                SimulatedRuntimeGroupCall::create("app-1"),
                SimulatedRuntimeGroupCall::ensure_absent("app-1"),
                SimulatedRuntimeGroupCall::remove_storage("app-1"),
            ],
            "retained evidence settles the teardown"
        );
        assert!(!fixture.support.storage_exists(&deployment_data("app-1")));
        assert!(
            fixture
                .state()
                .node_application_realization("app-1")
                .unwrap()
                .is_none()
        );
        assert!(fixture.current("app-1").is_none());
        // The confirmed deployment, and any later one reusing its identity, has no steps left.
        assert!(started("app-1").is_empty());
        assert_eq!(
            started("app-2"),
            [PreparationStep::ServiceImage {
                service: "web".to_owned()
            }]
        );
        let owner = node.status_owner();
        let section = owner
            .status_section(&HardwareCollection::default())
            .expect("status");
        assert!(
            section.deployments.is_empty(),
            "evidence stands in for the teardown"
        );
        assert_eq!(
            section.cleanup_evidence,
            [application_cleanup_evidence(
                "app-1",
                2,
                Some(launch_generation)
            )]
        );

        owner
            .accept_receipts(&BTreeSet::from([("app-1".to_owned(), 2)]))
            .expect("acknowledged");
        let section = owner
            .status_section(&HardwareCollection::default())
            .expect("status");
        assert!(section.cleanup_evidence.is_empty());
        assert_eq!(
            deployment_status(&section, "app-1").lifecycle,
            ApplicationLifecycleObservation::Reconciling as i32,
            "an acknowledged teardown still installed reports its own status"
        );
        node.reconcile_for_test()
            .await
            .expect("acknowledged teardown pass");
        assert_eq!(
            fixture.support.group_calls().len(),
            3,
            "never cleaned up again"
        );

        fixture.install(3, Vec::new());
        owner.retire_withdrawn_cleanup_evidence();
        assert!(
            fixture
                .state()
                .node_application_cleanup_evidence()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn linked_route_names_come_from_the_installed_assignment() {
        let fixture = NodeFixture::new(None);
        let mut linked = declaration("linked-1", 1, Present);
        linked.snapshot = Some(resolved_snapshot(application_with_route_input()));
        linked.placement = Some(node_placement());
        linked.snapshot.as_mut().unwrap().placement = Some(node_placement());
        linked.inference_route_id = Some("route-one".to_owned());
        let mut unknown = linked.clone();
        unknown.application_deployment_id = "unknown-1".to_owned();
        unknown.application_deployment_name = "unknown-1".to_owned();
        fixture.install(
            1,
            vec![
                assignment(linked, Some(route_name("chat-default"))),
                assignment(
                    unknown,
                    Some(ApplicationLinkedRouteProjection {
                        resolution: Some(application_linked_route_projection::Resolution::Unknown(
                            Default::default(),
                        )),
                    }),
                ),
            ],
        );
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("pass");

        let launch = fixture.launch("linked-1");
        assert_eq!(
            launch
                .units
                .values()
                .next()
                .unwrap()
                .environment
                .get("ROUTE_NAME")
                .map(String::as_str),
            Some("chat-default")
        );
        assert_eq!(
            fixture.support.group_calls(),
            [SimulatedRuntimeGroupCall::create("linked-1")]
        );
        assert_eq!(
            deployment_status(&status_of(&node), "unknown-1").failure_code,
            ApplicationStatusFailureCode::LinkedRouteUnknown as i32
        );
    }

    #[tokio::test]
    async fn node_logs_read_the_exact_pinned_group_and_give_explicit_outcomes() {
        let images = FakeImageStore::new([(DEV_TAG, FIRST_IMAGE)]);
        let fixture = NodeFixture::new(None).with_images(images);
        fixture.install(
            1,
            vec![assignment(local_declaration("dev-1", 1, Present), None)],
        );
        let node = fixture.realizer();
        let logs = node.log_owner();
        let linked_fixture = NodeFixture::new(None);
        let mut linked = declaration("linked-1", 1, Present);
        linked.snapshot = Some(resolved_snapshot(application_with_route_input()));
        linked.placement = Some(node_placement());
        linked.snapshot.as_mut().unwrap().placement = Some(node_placement());
        linked.inference_route_id = Some("route-one".to_owned());
        linked_fixture.install(
            1,
            vec![assignment(linked, Some(route_name("chat-default")))],
        );
        let linked_node = linked_fixture.realizer();

        let before_realization = logs
            .read_logs("dev-1", log_query(&[]))
            .await
            .expect("an unrealized assignment reads as not created");
        node.reconcile_for_test().await.expect("create pass");
        linked_node
            .reconcile_for_test()
            .await
            .expect("linked create pass");
        let generation = fixture.launch_generation("dev-1");
        let pinned = logs
            .read_logs("dev-1", log_query(&[]))
            .await
            .expect("history of the pinned group");
        let protected = linked_node
            .log_owner()
            .read_logs("linked-1", log_query(&[]))
            .await
            .expect("history of the linked group")
            .protected;
        let followed = logs
            .follow_logs(
                "dev-1",
                DeploymentLogFollowQuery::new(
                    Vec::new(),
                    BTreeMap::from([(
                        "web".to_owned(),
                        std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_020),
                    )]),
                    tokio_util::sync::CancellationToken::new(),
                ),
            )
            .await
            .expect("follow of the pinned group")
            .sources
            .pop()
            .expect("one source")
            .result
            .expect("an open source")
            .next()
            .await;
        let invalid = logs.read_logs("dev-1", log_query(&["node"])).await;
        fixture.install(
            2,
            vec![assignment(local_declaration("dev-1", 2, Teardown), None)],
        );
        let teardown = logs
            .read_logs("dev-1", log_query(&[]))
            .await
            .expect("a teardown reads as not created");
        fixture.install(3, Vec::new());
        let withdrawn = logs.read_logs("dev-1", log_query(&[])).await;

        let not_created = |sources: &OwnerLogSources<RuntimeLogSourceHistory>| {
            sources
                .sources
                .iter()
                .map(|history| {
                    (
                        history.source.name.clone(),
                        matches!(history.result, Err(RuntimeBackendError::WorkloadNotCreated)),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(not_created(&before_realization), [("web".to_owned(), true)]);
        let contents = |history: &RuntimeLogSourceHistory| {
            history
                .result
                .as_ref()
                .expect("a readable source")
                .entries
                .iter()
                .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
                .collect::<Vec<_>>()
        };
        // The present local image is the service's only step, written as its decision.
        assert_eq!(
            pinned
                .sources
                .iter()
                .map(|history| (history.source.name.as_str(), contents(history)))
                .collect::<Vec<_>>(),
            [
                (
                    "prepare:web:image",
                    vec![format!("Already present: {FIRST_IMAGE}")]
                ),
                (
                    "web",
                    [11, 12]
                        .map(|line| format!(
                            "simulated workload log line {line:03} owner=dev-1 generation={generation} unit=web"
                        ))
                        .to_vec()
                ),
            ]
        );
        let launched_values = linked_fixture
            .launch("linked-1")
            .units
            .values()
            .flat_map(|unit| unit.environment.values().cloned())
            .collect::<Vec<_>>();
        assert!(launched_values.contains(&"chat-default".to_owned()));
        assert_eq!(protected, ProtectedLogValues::new(launched_values));
        assert!(matches!(
            followed,
            Some(Ok(RuntimeLogEvent::Entry(entry)))
                if entry.content == format!(
                    "simulated workload log line 011 owner=dev-1 generation={generation} unit=web"
                ).into_bytes()
        ));
        assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
        assert_eq!(
            not_created(&teardown),
            [
                ("prepare:web:image".to_owned(), false),
                ("web".to_owned(), true)
            ]
        );
        assert!(matches!(withdrawn, Err(DeploymentLogError::NotFound)));
    }

    #[tokio::test]
    async fn node_logs_without_an_installed_view_serve_named_steps_from_the_store() {
        let fixture = NodeFixture::new(None);
        let logs = fixture.realizer().log_owner();
        fixture
            .preparation_logs()
            .writer(
                "dev-1",
                PreparationStep::ServiceBuild {
                    service: "web".to_owned(),
                },
            )
            .append(
                std::time::UNIX_EPOCH + Duration::from_secs(1),
                RuntimeLogStreamKind::Stdout,
                b"building web",
            );

        let read = logs
            .read_logs("dev-1", log_query(&["prepare:web:build", "web"]))
            .await
            .expect("named sources without an installed view");
        let unnamed = logs.read_logs("dev-1", log_query(&[])).await;

        assert_eq!(
            read.sources
                .iter()
                .map(|history| (
                    history.source.name.as_str(),
                    history
                        .result
                        .as_ref()
                        .map(|history| history
                            .entries
                            .iter()
                            .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
                            .collect::<Vec<_>>())
                        .ok()
                ))
                .collect::<Vec<_>>(),
            [
                ("prepare:web:build", Some(vec!["building web".to_owned()])),
                ("web", None),
            ]
        );
        assert!(matches!(
            read.sources[1].result,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
        assert!(matches!(unnamed, Err(DeploymentLogError::NotFound)));
    }

    #[tokio::test]
    async fn node_logs_serve_preparation_steps_before_the_group_exists_and_protect_launch_values() {
        let fixture = NodeFixture::new(None);
        let mut preparing = declaration("dev-1", 1, Present);
        preparing
            .configuration_attachments
            .push(ApplicationConfigurationAttachment {
                service_name: "web".to_owned(),
                binding_name: "token".to_owned(),
                environment_variable: "TOKEN".to_owned(),
                value: "secret-token".to_owned(),
            });
        // The service receives the password as a hash; the retained password itself is never
        // part of the launch, yet stays protected.
        let snapshot = preparing.snapshot.as_mut().expect("snapshot");
        snapshot
            .graph
            .as_mut()
            .expect("graph")
            .parameters
            .push(secret_parameter("password"));
        snapshot.parameter_values.push(ParameterValue {
            parameter_id: "password".to_owned(),
            value: Some(parameter_value::Value::String("secret-password".to_owned())),
        });
        preparing
            .secret_attachments
            .push(ApplicationSecretAttachment {
                service_name: "web".to_owned(),
                binding_name: "password".to_owned(),
                environment_variable: "PASSWORD_HASH".to_owned(),
                value: PASSWORD_HASH.to_owned(),
            });
        fixture.install(1, vec![assignment(preparing, None)]);
        // Never reconciled, so the deployment's group does not exist yet.
        let logs = fixture.realizer().log_owner();
        let preparation = fixture.preparation_logs();
        let time =
            |second: u64| std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + second);
        let build = preparation.writer(
            "dev-1",
            PreparationStep::ServiceBuild {
                service: "web".to_owned(),
            },
        );
        build.append(
            time(1),
            RuntimeLogStreamKind::Stderr,
            format!("building with secret-token, secret-password and {PASSWORD_HASH}").as_bytes(),
        );
        preparation
            .writer(
                "dev-1",
                PreparationStep::ServiceImage {
                    service: "web".to_owned(),
                },
            )
            .append(time(2), RuntimeLogStreamKind::Stdout, b"loading image");

        let all = logs
            .read_logs("dev-1", log_query(&[]))
            .await
            .expect("history mid-preparation");
        let filtered = logs
            .read_logs("dev-1", log_query(&["prepare:web:image"]))
            .await
            .expect("one step's history");
        let invalid = logs
            .read_logs("dev-1", log_query(&["prepare:db:build"]))
            .await;
        let followed = logs
            .follow_logs(
                "dev-1",
                DeploymentLogFollowQuery::new(
                    vec!["prepare:web:build".to_owned()],
                    BTreeMap::from([("prepare:web:build".to_owned(), time(1))]),
                    tokio_util::sync::CancellationToken::new(),
                ),
            )
            .await
            .expect("follow of one step");
        let mut follow = followed
            .sources
            .into_iter()
            .next()
            .expect("one source")
            .result
            .expect("an open step follow");
        build.append(time(3), RuntimeLogStreamKind::Stderr, b"built");
        let appended = follow.next().await;

        assert_eq!(
            all.sources
                .iter()
                .map(|history| (history.source.name.as_str(), history.source.label.as_str()))
                .collect::<Vec<_>>(),
            [
                ("prepare:web:build", "web build"),
                ("prepare:web:image", "web image"),
                ("web", "web"),
            ]
        );
        assert!(matches!(
            all.sources[2].result,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
        assert_eq!(
            all.protected,
            ProtectedLogValues::new([
                "secret-token".to_owned(),
                PASSWORD_HASH.to_owned(),
                "secret-password".to_owned(),
            ])
        );
        let build_entry = all.sources[0]
            .result
            .as_ref()
            .expect("build history")
            .entries[0]
            .clone();
        assert_eq!(
            filter_entry(build_entry, &all.protected).content,
            b"building with [REDACTED], [REDACTED] and [REDACTED]"
        );
        assert_eq!(
            filtered
                .sources
                .iter()
                .map(|history| history.source.name.as_str())
                .collect::<Vec<_>>(),
            ["prepare:web:image"]
        );
        assert_eq!(filtered.protected, all.protected);
        assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
        assert_eq!(followed.protected, all.protected);
        assert!(matches!(
            appended,
            Some(Ok(RuntimeLogEvent::Entry(entry))) if entry.content == b"built"
        ));
    }

    #[tokio::test]
    async fn a_follow_opened_before_the_group_exists_receives_each_container_redacted_as_it_starts()
    {
        let output = Arc::new(ContainerOutput::default());
        let fixture = NodeFixture::new(None).with_container_output(Arc::clone(&output));
        let mut application = application_with_route_input();
        let graph = application.graph.as_mut().expect("graph");
        let mut worker = graph.services[0].clone();
        worker.service_name = "worker".to_owned();
        worker.ports.clear();
        worker.configuration_bindings.clear();
        graph.services.push(worker);
        let mut linked = declaration("app-1", 1, Present);
        linked.snapshot = Some(resolved_snapshot(application));
        linked.placement = Some(node_placement());
        linked.snapshot.as_mut().unwrap().placement = Some(node_placement());
        linked.inference_route_id = Some("route-one".to_owned());
        // The route name reaches only the web container's environment, never an attachment.
        fixture.install(
            1,
            vec![assignment(linked, Some(route_name("secret-route")))],
        );
        let node = fixture.realizer();

        let mut follow = follow_every_source(&fixture, &node, "app-1").await;
        let opened = next_frames(&mut follow, 1).await;
        node.reconcile_for_test().await.expect("create pass");
        output.write("web", 1, "web serves secret-route");
        output.write("worker", 1, "worker ready");
        let announced = next_frames(&mut follow, 4).await;

        assert_eq!(
            opened,
            [
                ("web", "opened unavailable WorkloadNotCreated"),
                ("worker", "opened unavailable WorkloadNotCreated"),
            ]
            .map(|(source, frame)| (source.to_owned(), frame.to_owned()))
        );
        assert!(
            fixture.launch("app-1").units[&WorkloadUnitName("web".to_owned())]
                .environment
                .values()
                .any(|value| value == "secret-route"),
            "the route name is a value of the web container's environment"
        );
        assert_eq!(
            frames_of(&announced, "web"),
            ["opened", "entry web serves [REDACTED]"]
        );
        assert_eq!(
            frames_of(&announced, "worker"),
            ["opened", "entry worker ready"]
        );
    }

    #[tokio::test]
    async fn a_container_whose_creation_outcome_was_lost_is_announced_once_a_pass_sees_it() {
        let output = Arc::new(ContainerOutput::default());
        let fixture = NodeFixture::new(None)
            .with_container_output(Arc::clone(&output))
            .losing_creations(1);
        fixture.install(1, vec![assignment(declaration("app-1", 1, Present), None)]);
        let node = fixture.realizer();

        let mut follow = follow_every_source(&fixture, &node, "app-1").await;
        let opened = next_frames(&mut follow, 1).await;
        node.reconcile_for_test()
            .await
            .expect("creation pass whose outcome is lost");
        node.reconcile_for_test().await.expect("observing pass");
        output.write("web", 1, "first");
        let announced = next_frames(&mut follow, 2).await;

        assert_eq!(
            fixture.support.group_calls(),
            [SimulatedRuntimeGroupCall::create("app-1")],
            "the container is created once"
        );
        assert_eq!(
            [opened, announced].concat(),
            [
                "opened unavailable WorkloadNotCreated",
                "opened",
                "entry first"
            ]
            .map(|frame| ("web".to_owned(), frame.to_owned()))
        );
    }

    #[tokio::test]
    async fn a_container_restarted_in_place_resumes_without_repeats_until_removal_ends_the_follow()
    {
        let output = Arc::new(ContainerOutput::default());
        let fixture = NodeFixture::new(None).with_container_output(Arc::clone(&output));
        fixture.install(1, vec![assignment(declaration("app-1", 1, Present), None)]);
        let node = fixture.realizer();
        node.reconcile_for_test().await.expect("create pass");
        let launch = fixture.launch("app-1");

        let mut follow = follow_every_source(&fixture, &node, "app-1").await;
        output.write("web", 1, "first");
        output.write("web", 2, "second");
        let before_stop = next_frames(&mut follow, 3).await;
        // The host stops the container, as when its machine restarts.
        fixture
            .shared_runtime()
            .stop_workload_group(&launch)
            .await
            .expect("container stopped");
        let ended = next_frames(&mut follow, 1).await;
        node.reconcile_for_test().await.expect("restart pass");
        output.write("web", 3, "third");
        let restarted = next_frames(&mut follow, 2).await;
        fixture.install(2, vec![assignment(declaration("app-1", 2, Teardown), None)]);
        node.reconcile_for_test().await.expect("teardown pass");
        let removed = next_frames(&mut follow, 1).await;
        let after_removal = tokio::time::timeout(PROMPT, follow.next())
            .await
            .expect("the follow ends promptly");

        assert_eq!(
            fixture.support.group_calls(),
            [
                SimulatedRuntimeGroupCall::create("app-1"),
                SimulatedRuntimeGroupCall::stop("app-1"),
                SimulatedRuntimeGroupCall::start("app-1"),
                SimulatedRuntimeGroupCall::ensure_absent("app-1"),
                SimulatedRuntimeGroupCall::remove_storage("app-1"),
            ],
            "the container is started again in place, never recreated"
        );
        assert_eq!(
            [before_stop, ended, restarted, removed].concat(),
            [
                "opened",
                "entry first",
                "entry second",
                "ended",
                "opened",
                "entry third",
                "ended",
            ]
            .map(|frame| ("web".to_owned(), frame.to_owned()))
        );
        assert!(
            after_removal.is_none(),
            "removing the deployment ends its follow cleanly"
        );
    }

    fn log_query(sources: &[&str]) -> DeploymentLogReadQuery {
        DeploymentLogReadQuery::new(
            sources.iter().map(|source| (*source).to_owned()).collect(),
            RuntimeLogQuery::new(std::num::NonZeroU16::new(2).expect("limit"), None)
                .expect("query"),
        )
    }

    use ApplicationDeploymentDesiredState::{Paused, Present, Teardown};

    struct NodeFixture {
        directory: tempfile::TempDir,
        authority: AcceptedNodeViewAuthority,
        support: SimulatedRuntimeTestSupport,
        runtime: Arc<InMemoryRuntimeBackend>,
        state: Option<FileNodeState>,
        images: Option<FakeImageStore>,
        artifacts: Option<ArtifactManager>,
        output: Option<Arc<ContainerOutput>>,

        /// Group creations that act, then report a failure, as when their outcome is lost.
        lost_creations: Arc<std::sync::atomic::AtomicUsize>,

        /// Pin resolver used instead of the artifact manager's.
        launch_pins: Option<Arc<dyn super::LaunchPinResolver>>,

        /// Whether the runtime launches from references alone, like the simulated one.
        virtualized: bool,
    }

    impl NodeFixture {
        fn new(endpoint_port: Option<u16>) -> Self {
            let directory = crate::test_support::canonical_tempdir().expect("node directory");
            let state =
                FileNodeState::open(directory.path().join("node"), "site-one", "worker-one")
                    .expect("node state");
            let support = endpoint_port.map_or_else(
                || SimulatedRuntimeTestSupport::with_stop_failures(0),
                SimulatedRuntimeTestSupport::with_application_endpoint,
            );
            Self {
                runtime: runtime(&support),
                directory,
                authority: AcceptedNodeViewAuthority::new(
                    "site-one",
                    "node-one",
                    NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
                    ManagerSession::parse("a".repeat(32)).expect("session"),
                    LEASE,
                )
                .expect("view authority"),
                support,
                state: Some(state),
                images: None,
                artifacts: None,
                output: None,
                lost_creations: Arc::default(),
                launch_pins: None,
                virtualized: false,
            }
        }

        fn with_artifacts(mut self, artifacts: ArtifactManager) -> Self {
            self.artifacts = Some(artifacts);
            self
        }

        /// Pins with `resolver`, independently of any image preparation.
        fn with_launch_pins(mut self, resolver: Arc<dyn super::LaunchPinResolver>) -> Self {
            self.launch_pins = Some(resolver);
            self
        }

        /// Makes the runtime launch from references alone, so no image is prepared for it.
        fn virtualized(mut self) -> Self {
            self.virtualized = true;
            self
        }

        fn with_images(mut self, images: FakeImageStore) -> Self {
            self.images = Some(images);
            self
        }

        /// Serves each container's log from `output` instead of the simulated lines.
        fn with_container_output(mut self, output: Arc<ContainerOutput>) -> Self {
            self.output = Some(output);
            self
        }

        /// Makes the next `count` group creations act, then report a failure.
        fn losing_creations(self, count: usize) -> Self {
            self.lost_creations
                .store(count, std::sync::atomic::Ordering::SeqCst);
            self
        }

        /// The node's preparation logs, kept beside its node store.
        fn preparation_logs(&self) -> crate::deployment_logs::PreparationLogs {
            crate::deployment_logs::PreparationLogs::on_system_clock(self.directory.path())
        }

        fn realizer(&self) -> NodeApplicationRealizer {
            let mut runtimes = ApplicationRuntimeMapBuilder::leased(self.authority.clone());
            runtimes
                .insert_boxed("application-target-one", Box::new(self.shared_runtime()))
                .expect("node application target");
            let artifacts = self.artifacts.clone().or_else(|| {
                self.images.as_ref().map(|images| {
                    ArtifactManager::new(
                        self.directory.path().join("cache"),
                        Some(Arc::new(images.clone()) as Arc<dyn OciImagePreparer>),
                    )
                    .expect("artifact manager")
                })
            });
            NodeApplicationRealizer::new(NodeApplicationComposition {
                input: self.authority.application_input(),
                state: self.state().clone(),
                runtimes: runtimes.build(),
                launch_pins: self.launch_pins.clone().or_else(|| {
                    artifacts
                        .clone()
                        .map(|artifacts| Arc::new(artifacts) as Arc<dyn super::LaunchPinResolver>)
                }),
                artifact_manager: artifacts,
                publication_host: Ipv4Addr::LOCALHOST,
                preparation_logs: self.preparation_logs(),
                inventory: Arc::new(HardwareInventoryPublisher::new().1),
                timing: Timing::test().application,
                clock: Arc::new(SystemClock),
            })
            .expect("node realizer")
        }

        /// The node's runtime adapter, also used to act on containers as the host would.
        fn shared_runtime(&self) -> SharedRuntime {
            SharedRuntime(
                Arc::clone(&self.runtime),
                self.output.clone(),
                Arc::clone(&self.lost_creations),
                self.virtualized,
            )
        }

        /// Installs a complete view at `revision`, which also renews the action lease.
        fn install(&self, revision: u64, assignments: Vec<ApplicationAssignment>) {
            let projection = NodeViewProjection::new(
                vec![application_execution_target(), model_target()],
                Vec::new(),
            )
            .expect("projection")
            .with_application_assignments(assignments)
            .expect("application projection");
            self.authority
                .install_for_test("site-one", "node-one", revision, &projection)
                .expect("installed view");
        }

        /// Reopens the node state from its node store, as a restarted node process does.
        fn reopen_state(&mut self) {
            self.state.take();
            self.state = Some(
                FileNodeState::open(self.directory.path().join("node"), "site-one", "worker-one")
                    .expect("reopened node state"),
            );
        }

        fn state(&self) -> &FileNodeState {
            self.state.as_ref().expect("open node state")
        }

        /// Replaces the runtime with an empty one, as after the host lost its workload groups.
        fn reset_runtime(&mut self) {
            self.support = SimulatedRuntimeTestSupport::with_stop_failures(0);
            self.runtime = runtime(&self.support);
        }

        fn current(&self, id: &str) -> Option<CurrentApplicationDeployment> {
            self.state()
                .snapshot()
                .expect("current snapshot")
                .deployments
                .into_iter()
                .find(|record| record.application_deployment_id == id)
        }

        fn pins(&self, id: &str) -> Vec<NodeResolvedLaunchIdentity> {
            self.state()
                .node_application_realization(id)
                .expect("realization record")
                .map(|realization| realization.resolved_launch_identities)
                .unwrap_or_default()
        }

        fn launch(&self, id: &str) -> WorkloadGroupLaunch {
            self.support
                .group_launches()
                .into_iter()
                .rfind(|launch| launch.group_id.0 == id)
                .expect("group launch")
        }

        fn launch_generation(&self, id: &str) -> u64 {
            self.launch(id).correlation.reconcile_generation
        }

        fn launched_images(&self, id: &str) -> Vec<String> {
            let launches = self.runtime_launches(id);
            launches
                .iter()
                .flat_map(|launch| launch.units.values().map(|unit| unit.image.clone()))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        }

        fn runtime_launches(&self, id: &str) -> Vec<WorkloadGroupLaunch> {
            self.support
                .group_launches()
                .into_iter()
                .filter(|launch| launch.group_id.0 == id)
                .collect()
        }

        async fn group_running(&self, id: &str) -> Option<bool> {
            self.runtime
                .discover_workload_groups()
                .await
                .expect("Runtime discovery")
                .into_iter()
                .find_map(|observation| match observation {
                    WorkloadGroupObservation::Observed(group) if group.group_id.0 == id => Some(
                        group
                            .units
                            .values()
                            .all(|unit| unit.state == dotcore_runtime::WorkloadUnitState::Running),
                    ),
                    _ => None,
                })
        }
    }

    fn runtime(support: &SimulatedRuntimeTestSupport) -> Arc<InMemoryRuntimeBackend> {
        Arc::new(InMemoryRuntimeBackend::with_config(
            SimulatedRuntimeConfig {
                execution_platform: ExecutionPlatform::linux_amd64(),
                test_support: Some(support.clone()),
                ..Default::default()
            },
        ))
    }

    fn model_target() -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "model-target-one".to_owned(),
            site_node_id: "node-one".to_owned(),
            runtime_binding: Some(
                dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(
                    Default::default(),
                ),
            ),
            ..Default::default()
        }
    }

    fn node_placement() -> dotcore_proto::dotcore::deployment::v1::ExactPlacement {
        dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "application-target-one".to_owned(),
        }
    }

    fn declaration(
        id: &str,
        lifecycle_generation: u64,
        desired_state: ApplicationDeploymentDesiredState,
    ) -> ApplicationDeploymentDeclaration {
        let mut declaration = deployment(desired_state);
        declaration.application_deployment_id = id.to_owned();
        declaration.application_deployment_name = id.to_owned();
        declaration.lifecycle_generation = lifecycle_generation;
        let service = &mut declaration
            .snapshot
            .as_mut()
            .unwrap()
            .graph
            .as_mut()
            .unwrap()
            .services[0];
        service.readiness = Some(ApplicationServiceReadiness {
            check: Some(application_service_readiness::Check::HttpGet(
                ApplicationHttpReadiness {
                    port_name: "http".to_owned(),
                    path: "/".to_owned(),
                },
            )),
        });
        declaration
    }

    /// Gives the first service of a declaration one deployment-scoped data binding.
    fn with_deployment_data(
        mut declaration: ApplicationDeploymentDeclaration,
    ) -> ApplicationDeploymentDeclaration {
        let service = &mut declaration
            .snapshot
            .as_mut()
            .unwrap()
            .graph
            .as_mut()
            .unwrap()
            .services[0];
        service.storage_bindings = vec![StorageBinding {
            binding_name: "data".to_owned(),
            mount_path: "/data".to_owned(),
            kind: StorageKind::Data as i32,
            scope: StorageScope::Deployment as i32,
            environment: Vec::new(),
        }];
        declaration.storage_attachments = vec![ApplicationStorageAttachment {
            service_name: service.service_name.clone(),
            binding_name: "data".to_owned(),
            storage_name: "web-data".to_owned(),
        }];
        declaration
    }

    /// A node declaration whose only service is a local-development tag left for the node.
    fn local_declaration(
        id: &str,
        lifecycle_generation: u64,
        desired_state: ApplicationDeploymentDesiredState,
    ) -> ApplicationDeploymentDeclaration {
        let mut declaration = declaration(id, lifecycle_generation, desired_state);
        let snapshot = declaration.snapshot.as_mut().unwrap();
        let reference = snapshot.graph.as_mut().unwrap().services[0]
            .artifact
            .as_mut()
            .unwrap()
            .oci_mut()
            .unwrap();
        reference.digest = None;
        reference.tag = Some("dev".to_owned());
        let selection = &mut snapshot.service_selections[0];
        selection.preparation_policy = ArtifactPreparationPolicy::LocalDevelopment as i32;
        selection.launch_identity.clear();
        declaration
    }

    /// Declares the first service as Docker image derivation package `apps.web.docker` of
    /// `flake`, with its catalog `store_paths`, leaving its launch identity to the node.
    fn docker_delivered_declaration(
        id: &str,
        lifecycle_generation: u64,
        desired_state: ApplicationDeploymentDesiredState,
        flake: &str,
        store_paths: &[(&str, &str)],
    ) -> ApplicationDeploymentDeclaration {
        use dotcore_proto::dotcore::catalog::v1::{
            NixArtifact, NixDockerImage, NixSystemStorePath,
            application_service_artifact::Kind as ServiceArtifactKind, nix_artifact::Provides,
        };

        let mut declaration = declaration(id, lifecycle_generation, desired_state);
        let snapshot = declaration.snapshot.as_mut().unwrap();
        snapshot.graph.as_mut().unwrap().services[0]
            .artifact
            .as_mut()
            .unwrap()
            .kind = Some(ServiceArtifactKind::Nix(NixArtifact {
            flake: flake.to_owned(),
            package: Some("apps.web.docker".to_owned()),
            provides: Some(Provides::DockerImage(NixDockerImage {})),
            store_paths: store_paths
                .iter()
                .map(|(system, store_path)| NixSystemStorePath {
                    system: (*system).to_owned(),
                    store_path: (*store_path).to_owned(),
                })
                .collect(),
        }));
        let selection = &mut snapshot.service_selections[0];
        selection.preparation_policy = if flake == "self" {
            ArtifactPreparationPolicy::DevelopmentSource
        } else {
            ArtifactPreparationPolicy::ImmutableRemote
        } as i32;
        selection.launch_identity.clear();
        declaration
    }

    fn assignment(
        declaration: ApplicationDeploymentDeclaration,
        linked_route: Option<ApplicationLinkedRouteProjection>,
    ) -> ApplicationAssignment {
        ApplicationAssignment {
            declaration: Some(declaration),
            linked_route,
        }
    }

    fn route_name(name: &str) -> ApplicationLinkedRouteProjection {
        ApplicationLinkedRouteProjection {
            resolution: Some(application_linked_route_projection::Resolution::RouteName(
                name.to_owned(),
            )),
        }
    }

    fn pin(key: &str, identity: &str) -> NodeResolvedLaunchIdentity {
        NodeResolvedLaunchIdentity {
            selection_key: key.to_owned(),
            launch_identity: identity.to_owned(),
        }
    }

    fn status_of(node: &NodeApplicationRealizer) -> ApplicationControllerStatusSection {
        node.status_owner()
            .status_section(&HardwareCollection::default())
            .expect("application status")
    }

    /// Returns the data the launch of one deployment attaches under its deployment scope.
    fn deployment_data(deployment_id: &str) -> WorkloadStorageKey {
        WorkloadStorageKey {
            kind: WorkloadStorageKind::Data,
            namespace: crate::storage::DEPLOYMENT_NAMESPACE.to_owned(),
            key: deployment_id.to_owned(),
        }
    }

    fn deployment_status(
        section: &ApplicationControllerStatusSection,
        id: &str,
    ) -> ApplicationDeploymentStatus {
        section
            .deployments
            .iter()
            .find(|status| status.application_deployment_id == id)
            .cloned()
            .expect("deployment status")
    }

    fn memory_deployment(accelerator_device_ids: Vec<String>) -> ApplicationDeploymentDeclaration {
        ApplicationDeploymentDeclaration {
            application_deployment_id: "memory-1".to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: SiteNodeId::for_test("node-a").to_string(),
                execution_target_id: "target-a".to_owned(),
            }),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                service_selections: vec![
                    ResolvedApplicationServiceSelection {
                        service_name: "cache".to_owned(),
                        ..Default::default()
                    },
                    ResolvedApplicationServiceSelection {
                        service_name: "web".to_owned(),
                        ..Default::default()
                    },
                ],
                accelerator_device_ids,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn memory_target(device_ids: Vec<String>) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "target-a".to_owned(),
            display_name: "target-a".to_owned(),
            site_node_id: SiteNodeId::for_test("node-a").to_string(),
            accelerator_device_ids: device_ids,
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                        DockerApplicationRuntimeAdapter::default(),
                    )),
                },
            )),
            platform: None,
        }
    }

    fn memory_identity(
        node_id: &str,
        process_id: u32,
        process_start_token: u64,
    ) -> HostProcessIdentity {
        HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test(node_id),
            process_id,
            process_start_token,
        }
    }

    fn memory_group(units: &[(&str, Option<HostProcessIdentity>)]) -> WorkloadGroupObservation {
        WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
            group_id: WorkloadGroupId("memory-1-group".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "memory-1".to_owned(),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId("simulated".to_owned()),
            units: units
                .iter()
                .map(|(name, identity)| {
                    (
                        WorkloadUnitName(name.to_string()),
                        WorkloadUnitObservation {
                            state: WorkloadUnitState::Running,
                            ready_launch_gate: None,
                            host_process: identity.clone(),
                        },
                    )
                })
                .collect(),
            assigned_endpoint: None,
        })
    }

    fn memory_process(
        device_id: &str,
        process_id: u32,
        process_start_token: u64,
        used_memory_bytes: u64,
    ) -> ObservedAcceleratorProcess {
        ObservedAcceleratorProcess {
            device_id: device_id.to_owned(),
            process_id,
            process_start_token,
            ancestors: Vec::new(),
            used_memory_bytes: Some(used_memory_bytes),
        }
    }

    fn publish_memory(
        publisher: &HardwareInventoryPublisher,
        vendor: AcceleratorVendor,
        rows: Vec<ObservedAcceleratorProcess>,
    ) {
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(memory_timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: AcceleratorProcessObservation::Observed(vec![
                        AcceleratorVendorProcessObservation {
                            vendor,
                            result: AcceleratorVendorProcessResult::Observed(rows),
                        },
                    ]),
                },
                memory_timestamp(11),
            ),
        );
    }

    fn memory_timestamp(seconds: i64) -> dotcore_proto::google::protobuf::Timestamp {
        dotcore_proto::google::protobuf::Timestamp { seconds, nanos: 0 }
    }

    fn memory_refresh() -> AcceleratorMemoryRefreshState {
        AcceleratorMemoryRefreshState {
            attempted_at: Some(memory_timestamp(11)),
            outcome: Some(accelerator_memory_refresh_state::Outcome::Succeeded(
                dotcore_proto::google::protobuf::Empty {},
            )),
        }
    }

    fn memory_available(used_memory_bytes: u64) -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: Some(memory_timestamp(10)),
            refresh: Some(memory_refresh()),
            result: Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: vec![AcceleratorMemoryDeviceUsage {
                        managed_node_id: SiteNodeId::for_test("node-a").to_string(),
                        device_id: "GPU-a".to_owned(),
                        used_memory_bytes,
                    }],
                },
            )),
        }
    }

    fn memory_unavailable(
        code: &str,
        message: &str,
        refresh: Option<AcceleratorMemoryRefreshState>,
    ) -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: None,
            refresh,
            result: Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: code.to_owned(),
                    message: message.to_owned(),
                },
            )),
        }
    }

    fn memory_not_applicable() -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::NotApplicable(
                dotcore_proto::google::protobuf::Empty {},
            )),
        }
    }

    /// Local HTTP endpoint answering every readiness request with 200.
    struct ReadyEndpoint {
        port: u16,
        _task: tokio::task::JoinHandle<()>,
    }

    impl ReadyEndpoint {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("readiness listener");
            let port = listener.local_addr().expect("listener address").port();
            let task = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                while let Ok((mut stream, _)) = listener.accept().await {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
                }
            });
            Self { port, _task: task }
        }
    }

    /// Image preparer that holds each preparation until the test releases it.
    #[derive(Default)]
    struct GatedImagePreparer {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl OciImagePreparer for GatedImagePreparer {
        fn prepare(
            &self,
            _: OciPreparationRequest,
            _: tokio_util::sync::CancellationToken,
            _: dotcore_artifact::StepOutput,
        ) -> OciPreparationFuture {
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            })
        }
    }

    /// Node-local image store: tags resolve to image IDs, and only present IDs prepare.
    #[derive(Clone)]
    struct FakeImageStore {
        state: Arc<Mutex<FakeImages>>,
    }

    struct FakeImages {
        tags: BTreeMap<String, String>,
        images: BTreeSet<String>,
        resolutions: usize,
    }

    impl FakeImageStore {
        fn new<const N: usize>(tags: [(&str, &str); N]) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeImages {
                    tags: tags
                        .iter()
                        .map(|(tag, id)| ((*tag).to_owned(), (*id).to_owned()))
                        .collect(),
                    images: tags.iter().map(|(_, id)| (*id).to_owned()).collect(),
                    resolutions: 0,
                })),
            }
        }

        fn retag(&self, tag: &str, image_id: &str) {
            let mut state = self.state.lock().unwrap();
            state.tags.insert(tag.to_owned(), image_id.to_owned());
            state.images.insert(image_id.to_owned());
        }

        fn remove_image(&self, image_id: &str) {
            self.state.lock().unwrap().images.remove(image_id);
        }

        fn resolutions(&self) -> usize {
            self.state.lock().unwrap().resolutions
        }
    }

    impl OciImagePreparer for FakeImageStore {
        fn prepare(
            &self,
            request: OciPreparationRequest,
            _: tokio_util::sync::CancellationToken,
            _: dotcore_artifact::StepOutput,
        ) -> OciPreparationFuture {
            let present = self
                .state
                .lock()
                .unwrap()
                .images
                .contains(&request.reference);
            Box::pin(std::future::ready(if present {
                Ok(())
            } else {
                Err(OciPreparationError::LocalImageMissing)
            }))
        }

        fn as_local_image_identity_resolver(&self) -> Option<&dyn LocalImageIdentityResolver> {
            Some(self)
        }
    }

    impl LocalImageIdentityResolver for FakeImageStore {
        fn resolve_local_image_identity(
            &self,
            reference: String,
        ) -> LocalImageIdentityResolutionFuture {
            let mut state = self.state.lock().unwrap();
            state.resolutions += 1;
            let resolved = state
                .tags
                .get(&reference)
                .cloned()
                .ok_or(LocalImageIdentityResolutionError::LocalImageMissing);
            Box::pin(std::future::ready(resolved))
        }
    }

    /// Node runtime adapter over one in-memory runtime that outlives a realizer restart, with
    /// the container output its group logs serve when a test writes it. It needs images prepared
    /// on the host unless built virtualized.
    struct SharedRuntime(
        Arc<InMemoryRuntimeBackend>,
        Option<Arc<ContainerOutput>>,
        Arc<std::sync::atomic::AtomicUsize>,
        bool,
    );

    impl WorkloadGroupBackend for SharedRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            WorkloadGroupBackend::backend_id(&*self.0)
        }

        fn materialization_requirements(
            &self,
        ) -> dotcore_runtime::WorkloadMaterializationRequirements {
            if self.3 {
                WorkloadGroupBackend::materialization_requirements(&*self.0)
            } else {
                dotcore_runtime::WorkloadMaterializationRequirements::physical()
            }
        }

        fn execution_platform<'a>(&'a self, id: &'a str) -> RuntimeFuture<'a, ExecutionPlatform> {
            self.0.execution_platform(id)
        }

        fn discover_workload_groups<'a>(
            &'a self,
        ) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
            self.0.discover_workload_groups()
        }

        fn storage_exists<'a>(&'a self, key: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
            self.0.storage_exists(key)
        }

        fn remove_storage<'a>(&'a self, key: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
            self.0.remove_storage(key)
        }

        fn ensure_workload_group_absent<'a>(
            &'a self,
            id: &'a WorkloadGroupId,
            owner_id: &'a str,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(async move {
                self.0.ensure_workload_group_absent(id, owner_id).await?;
                if let Some(output) = &self.1 {
                    output.remove();
                }
                Ok(())
            })
        }

        fn create_workload_group<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
            Box::pin(async move {
                let observation = self.0.create_workload_group(group).await?;
                let lost = self
                    .2
                    .try_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |remaining| remaining.checked_sub(1),
                    )
                    .is_ok();
                if lost {
                    return Err(RuntimeBackendError::Docker(anyhow::anyhow!(
                        "the creation outcome was lost"
                    )));
                }
                Ok(observation)
            })
        }

        fn start_workload_group<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
            self.0.start_workload_group(group)
        }

        fn inspect_workload_group<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
            self.0.inspect_workload_group(group)
        }

        fn remove_workload_group<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(async move {
                self.0.remove_workload_group(group).await?;
                if let Some(output) = &self.1 {
                    output.remove();
                }
                Ok(())
            })
        }

        fn remove_observed_workload_group<'a>(
            &'a self,
            group: &'a ObservedWorkloadGroup,
        ) -> RuntimeFuture<'a, ()> {
            self.0.remove_observed_workload_group(group)
        }

        fn stop_workload_group<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(async move {
                self.0.stop_workload_group(group).await?;
                if let Some(output) = &self.1 {
                    output.stop();
                }
                Ok(())
            })
        }

        fn read_workload_group_logs<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
            sources: &'a RuntimeLogSourceSelection,
            query: &'a RuntimeLogQuery,
        ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceHistory>> {
            self.0.read_workload_group_logs(group, sources, query)
        }

        fn follow_workload_group_logs<'a>(
            &'a self,
            group: &'a WorkloadGroupLaunch,
            sources: &'a RuntimeLogSourceSelection,
            query: RuntimeGroupLogFollowQuery,
        ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceStream>> {
            Box::pin(async move {
                let streams = self
                    .0
                    .follow_workload_group_logs(group, sources, query)
                    .await?;
                let Some(output) = &self.1 else {
                    return Ok(streams);
                };
                Ok(streams
                    .into_iter()
                    .map(|stream| RuntimeLogSourceStream {
                        result: stream.result.map(|_| output.follow(&stream.source)),
                        source: stream.source,
                    })
                    .collect())
            })
        }
    }

    /// Container output a test writes per unit, followed as Docker serves it: a follow replays
    /// the container's whole output whatever the resume position, then streams new lines until
    /// the container stops. A restart in place keeps the output; removal discards it.
    #[derive(Default)]
    struct ContainerOutput(Mutex<BTreeMap<String, UnitOutput>>);

    #[derive(Default)]
    struct UnitOutput {
        entries: Vec<RuntimeLogEntry>,
        followers:
            Vec<tokio::sync::mpsc::UnboundedSender<Result<RuntimeLogEvent, RuntimeBackendError>>>,
    }

    impl ContainerOutput {
        /// Writes one stdout line of `unit` at `second` past a fixed epoch.
        fn write(&self, unit: &str, second: u64, content: &str) {
            let entry = RuntimeLogEntry {
                source: RuntimeLogSource {
                    name: unit.to_owned(),
                    label: unit.to_owned(),
                },
                timestamp: std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + second),
                stream: RuntimeLogStreamKind::Stdout,
                content: content.as_bytes().to_vec(),
                truncated: false,
            };
            let mut units = self.0.lock().expect("container output");
            let output = units.entry(unit.to_owned()).or_default();
            output.followers.retain(|follower| {
                follower
                    .send(Ok(RuntimeLogEvent::Entry(entry.clone())))
                    .is_ok()
            });
            output.entries.push(entry);
        }

        fn follow(&self, source: &RuntimeLogSource) -> RuntimeLogStream {
            let (follower, stream) = tokio::sync::mpsc::unbounded_channel();
            let mut units = self.0.lock().expect("container output");
            let output = units.entry(source.name.clone()).or_default();
            for entry in &output.entries {
                follower
                    .send(Ok(RuntimeLogEvent::Entry(entry.clone())))
                    .expect("an open follow");
            }
            output.followers.push(follower);
            Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(stream))
        }

        /// Ends every follow, as the containers stop.
        fn stop(&self) {
            for output in self.0.lock().expect("container output").values_mut() {
                output.followers.clear();
            }
        }

        /// Ends every follow and discards the output, as the containers are removed.
        fn remove(&self) {
            self.0.lock().expect("container output").clear();
        }
    }

    /// Opens the node's follow of every source of `deployment_id`, as the manager does.
    async fn follow_every_source(
        fixture: &NodeFixture,
        node: &NodeApplicationRealizer,
        deployment_id: &str,
    ) -> impl tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>> + Unpin
    {
        let reader = DeploymentLogReader::new(
            fixture.authority.clone(),
            DeploymentLogOwners {
                // No model deployment is asked for here.
                model: node.log_owner(),
                application: Some(node.log_owner()),
            },
            ManagementLogAdmission::new(),
        );
        reader
            .follow(FollowDeploymentLogsRequest {
                target: Some(DeploymentLogTarget {
                    manager_session_id: "a".repeat(32),
                    node_incarnation_id: "1".repeat(32),
                    owner_kind: DeploymentOwnerKind::Application as i32,
                    deployment_id: deployment_id.to_owned(),
                }),
                source_names: Vec::new(),
                resume_positions: Vec::new(),
            })
            .await
            .expect("an admitted follow")
    }

    /// Waits for the next `count` frames, each described per source it names.
    async fn next_frames(
        follow: &mut (
                 impl tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>>
                 + Unpin
             ),
        count: usize,
    ) -> Vec<(String, String)> {
        let mut frames = Vec::new();
        while frames.len() < count {
            let frame = tokio::time::timeout(PROMPT, follow.next())
                .await
                .expect("a frame arrives promptly")
                .expect("an open follow")
                .expect("a follow frame")
                .frame
                .expect("a frame payload");
            frames.extend(describe_frame(frame));
        }
        frames
    }

    /// The frames described for one source, in order.
    fn frames_of(frames: &[(String, String)], source_name: &str) -> Vec<String> {
        frames
            .iter()
            .filter(|(source, _)| source == source_name)
            .map(|(_, frame)| frame.clone())
            .collect()
    }

    fn describe_frame(frame: follow_deployment_logs_response::Frame) -> Vec<(String, String)> {
        use follow_deployment_logs_response::Frame;

        let opening = |opening: DeploymentLogSourceOpening| {
            let source = opening.source.expect("an opened source").name;
            match opening.unavailable {
                Some(reason) => (
                    source,
                    format!(
                        "opened unavailable {:?}",
                        DeploymentLogUnavailableReason::try_from(reason).expect("a known reason")
                    ),
                ),
                None => (source, "opened".to_owned()),
            }
        };
        match frame {
            Frame::Opened(opened) => opened.sources.into_iter().map(opening).collect(),
            Frame::SourceOpened(opened) => vec![opening(opened)],
            Frame::Entry(entry) => vec![(
                entry.source_name,
                format!(
                    "entry {}",
                    String::from_utf8_lossy(&entry.entry.expect("an entry").content)
                ),
            )],
            Frame::SourceEnded(ended) => vec![(ended.source_name, "ended".to_owned())],
            Frame::SourceUnavailable(unavailable) => {
                vec![(unavailable.source_name, "unavailable".to_owned())]
            }
            Frame::Rejected(rejected) => panic!("follow rejected: {rejected:?}"),
        }
    }
}
