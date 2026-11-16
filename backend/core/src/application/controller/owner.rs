//! Manager owner of application deployment intent behind the application service.
//!
//! It is the only holder of declaration mutation authority: it reserves a never-reused lifecycle
//! generation before exposing each create, pause, resume, or teardown, projects public status
//! from declarations and current records, and removes a declaration only when it still matches
//! the finalized teardown, for example generation 4 in teardown on SiteNode `node-one`.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
};

use dotcore_catalog::Catalog;
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ApplicationDeploymentState, ApplicationSelectedInferenceRoute,
        ListApplicationDeploymentsResponse,
    },
    site::v1::Site,
};
use dotcore_site::node::{
    control::{
        ApplicationStatusEvidence, DeploymentStatusEvidence, ManagerNodeControlCoordinator,
        NodeDeploymentReference,
    },
    target_inventory::TargetCapabilitySource,
};
use tokio::sync::Mutex as AsyncMutex;

use super::{
    ApplicationConfigurationSources, ApplicationLifecycleError, InferenceRouteResolver,
    accelerator_memory::node_hosted_projection, configuration, current_error, declaration_error,
    desired_state, eligible_target, project,
};
use crate::application::repository::{
    ApplicationDeclarationRepository, ApplicationLifecycleGenerations, ApplicationRepositoryError,
    CurrentApplicationRepository,
};

/// Manager owner of application deployment intent.
///
/// It is the only holder of the declaration repository: it accepts and mutates declarations,
/// reserves each lifecycle generation before exposing intent, projects public status, and removes
/// a declaration only through conditional teardown finalization.
pub(super) struct ApplicationOwner<D, S> {
    pub(super) catalog: Arc<Catalog>,
    pub(super) configuration_sources: ApplicationConfigurationSources,
    pub(super) inference_route_resolver: Arc<dyn InferenceRouteResolver>,
    pub(super) target_capabilities: Arc<dyn TargetCapabilitySource>,
    pub(super) secret_generator: Arc<dyn crate::SecretGenerator>,
    pub(super) desired: Arc<D>,
    pub(super) current: Arc<S>,

    /// Serializes owner operations and projections.
    pub(super) operation: AsyncMutex<()>,

    /// Manager node-control authority over node status freshness, installed after composition.
    pub(super) remote_status: RwLock<Option<ManagerNodeControlCoordinator>>,

    /// Manager reader of deployment logs on their hosting nodes, installed after composition.
    pub(super) node_logs: RwLock<Option<crate::deployment_logs::NodeDeploymentLogs>>,
}

impl<D, S> ApplicationOwner<D, S>
where
    D: ApplicationDeclarationRepository,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations,
{
    /// Reserves the next never-reused lifecycle generation for one deployment.
    pub(super) fn reserve_lifecycle_generation(
        &self,
        deployment_id: &str,
    ) -> Result<u64, ApplicationLifecycleError> {
        self.current
            .reserve_lifecycle_generation(deployment_id)
            .map_err(current_error)
    }

    /// Rejects retained non-teardown declarations whose exact placement names no eligible target of this owner; platform and device drift is left to the reconcile pass.
    pub(super) fn validate_retained_deployments(&self) -> Result<(), ApplicationLifecycleError> {
        let declaration_snapshot = self.desired.snapshot().map_err(declaration_error)?;
        let current_snapshot = self.current.snapshot().map_err(current_error)?;
        let site = self.application_site()?;
        if declaration_snapshot.site_id != site.site_id || current_snapshot.site_id != site.site_id
        {
            return Err(configuration(
                "Application repository site ID does not match desired site",
            ));
        }
        for deployment in &declaration_snapshot.deployments {
            if desired_state(deployment)? != ApplicationDeploymentDesiredState::Teardown {
                let placement = deployment.placement.as_ref().ok_or_else(|| {
                    configuration("Application deployment has no exact placement")
                })?;
                eligible_target(&site, placement)?;
            }
        }
        Ok(())
    }

    /// Removes current records left without a declaration, as after an interrupted teardown
    /// finalization; deployment identities are never reused, so no later declaration needs them.
    pub(super) fn prune_undeclared_current_records(&self) -> Result<(), ApplicationLifecycleError> {
        let declared = self
            .desired
            .snapshot()
            .map_err(declaration_error)?
            .deployments
            .into_iter()
            .map(|deployment| deployment.application_deployment_id)
            .collect::<BTreeSet<_>>();
        let undeclared = self
            .current
            .snapshot()
            .map_err(current_error)?
            .deployments
            .into_iter()
            .filter(|record| !declared.contains(&record.application_deployment_id))
            .map(|record| record.application_deployment_id);
        for deployment_id in undeclared {
            self.remove_current_record(&deployment_id)?;
        }
        Ok(())
    }

    pub(super) fn application_site(&self) -> Result<Site, ApplicationLifecycleError> {
        let snapshot = self
            .desired
            .declaration_snapshot()
            .map_err(declaration_error)?;
        Ok(Site {
            site_id: snapshot.site_id,
            nodes: snapshot.nodes,
            execution_targets: snapshot.execution_targets,
            application_deployments: snapshot.deployments,
            ..Default::default()
        })
    }

    pub(super) fn inference_route_projection(
        &self,
        declaration: &ApplicationDeploymentDeclaration,
    ) -> Result<Option<ApplicationSelectedInferenceRoute>, ApplicationLifecycleError> {
        declaration
            .inference_route_id
            .as_deref()
            .map(|inference_route_id| {
                self.inference_route_resolver
                    .project_current(inference_route_id)
                    .map(|route| ApplicationSelectedInferenceRoute {
                        inference_route_id: route.inference_route_id,
                        route_name: route.route_name,
                    })
                    .map_err(ApplicationLifecycleError::InferenceRoute)
            })
            .transpose()
    }

    /// Projects every declaration with its current record. A deployment keeps its last
    /// observation, marked stale, while its hosting node has no current status, or while that
    /// status does not list it yet and the record came from an earlier report. Unit accelerator
    /// memory comes only from the hosting node's current status, never from the record.
    pub(super) fn projection(
        &self,
    ) -> Result<ListApplicationDeploymentsResponse, ApplicationLifecycleError> {
        let declarations = self
            .desired
            .declaration_snapshot()
            .map_err(declaration_error)?;
        // Status acceptance writes the current record before it records the status, so reading
        // evidence first guarantees that a status seen as current is reflected in the record.
        let evidence = self.status_evidence(&declarations.deployments);
        let states = self
            .current
            .snapshot()
            .map_err(current_error)?
            .deployments
            .into_iter()
            .map(|record| (record.application_deployment_id.clone(), record))
            .collect::<BTreeMap<_, _>>();
        let deployments = declarations
            .deployments
            .iter()
            .filter_map(|deployment| {
                let record = states.get(&deployment.application_deployment_id);
                // Finalization removes the current record only after the declaration is durably
                // gone, so a teardown declaration without a record has already finished.
                if record.is_none()
                    && deployment.desired_state
                        == ApplicationDeploymentDesiredState::Teardown as i32
                {
                    return None;
                }
                Some((deployment, record))
            })
            .map(|(deployment, record)| {
                let (record, mut state) = record
                    .and_then(|record| Some((record, record.state.clone()?)))
                    .ok_or_else(|| {
                        configuration(
                            "Application deployment declaration has no current-state record",
                        )
                    })?;
                let node = evidence.get(deployment.application_deployment_id.as_str());
                state.observation_stale = match node.map(|node| node.evidence) {
                    None | Some(DeploymentStatusEvidence::Current) => false,
                    // Only the record an accepted mutation wrote stands in for the node's
                    // acknowledgement; a record from an earlier report is not evidence.
                    Some(DeploymentStatusEvidence::AcknowledgementPending) => {
                        record.reconciled_lifecycle_generation.is_some()
                    }
                    Some(DeploymentStatusEvidence::Unavailable) => true,
                };
                let mut projected = project(
                    deployment,
                    state,
                    self.inference_route_projection(deployment)?,
                )?;
                projected.service_accelerator_memory = node_hosted_projection(
                    deployment,
                    projected
                        .state
                        .as_ref()
                        .map(ApplicationDeploymentState::lifecycle_state)
                        .unwrap_or_default(),
                    &declarations.execution_targets,
                    node.map(|node| node.accelerator_memory.clone())
                        .unwrap_or_default(),
                );
                Ok(projected)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ListApplicationDeploymentsResponse { deployments })
    }

    /// Classifies each node-hosted deployment against its hosting node's status. Without
    /// node-control authority every node-hosted deployment is unavailable.
    fn status_evidence<'a>(
        &self,
        declarations: &'a [ApplicationDeploymentDeclaration],
    ) -> BTreeMap<&'a str, ApplicationStatusEvidence> {
        let remote = declarations
            .iter()
            .filter_map(|declaration| {
                let placement = declaration.placement.as_ref()?;
                Some(NodeDeploymentReference {
                    node_id: &placement.site_node_id,
                    deployment_id: &declaration.application_deployment_id,
                    lifecycle_generation: declaration.lifecycle_generation,
                })
            })
            .collect::<Vec<_>>();
        if remote.is_empty() {
            return BTreeMap::new();
        }
        let evidence = match self
            .remote_status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            Some(coordinator) => coordinator.application_status_evidence(&remote),
            None => vec![
                ApplicationStatusEvidence {
                    evidence: DeploymentStatusEvidence::Unavailable,
                    accelerator_memory: Vec::new(),
                };
                remote.len()
            ],
        };
        remote
            .iter()
            .map(|deployment| deployment.deployment_id)
            .zip(evidence)
            .collect()
    }

    fn remove_current_record(&self, deployment_id: &str) -> Result<(), ApplicationLifecycleError> {
        let existing = self
            .current
            .snapshot()
            .map_err(current_error)?
            .deployments
            .into_iter()
            .find(|record| record.application_deployment_id == deployment_id);
        if let Some(existing) = existing {
            match self.current.remove(deployment_id, existing.record_revision) {
                Ok(_)
                | Err(
                    ApplicationRepositoryError::NotFound
                    | ApplicationRepositoryError::RevisionConflict,
                ) => {}
                Err(error) => return Err(current_error(error)),
            }
        }
        Ok(())
    }
}
