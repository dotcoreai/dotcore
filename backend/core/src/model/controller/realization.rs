//! Provides the node-local result boundary through which a model realizer returns cleanup
//! evidence.
//!
//! Confirming the cleanup of a torn-down deployment removes its preparation logs, ends the
//! node's log follows of it, and records durable evidence in the node store, which the node
//! reports to the manager until it is acknowledged, so a realizer never receives declaration
//! mutation authority.

use dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration;
use dotcore_site::store::FileNodeState;

use tracing::warn;

use super::{ModelLifecycleError, ModelRealizationResults};
use crate::deployment_logs::{PreparationLogs, PreparationOwner, WorkloadLifecycle};

/// Node-local result sink backed by the durable node store. Confirming cleanup removes the
/// deployment's preparation logs, then its current record and records its evidence in one state
/// transaction, and once confirmed tells the deployment's log follows it is removed. A log
/// removal failure is reported but never blocks the confirmation.
#[derive(Clone)]
pub(crate) struct DurableModelRealizationResults {
    state: FileNodeState,
    preparation_logs: PreparationLogs,
    workload_lifecycle: WorkloadLifecycle,
}

impl DurableModelRealizationResults {
    pub(crate) fn new(
        state: FileNodeState,
        preparation_logs: PreparationLogs,
        workload_lifecycle: WorkloadLifecycle,
    ) -> Self {
        Self {
            state,
            preparation_logs,
            workload_lifecycle,
        }
    }
}

impl ModelRealizationResults for DurableModelRealizationResults {
    fn confirm_cleanup(
        &self,
        deployment: &ModelDeploymentDeclaration,
        realization_generation: Option<u64>,
    ) -> Result<(), ModelLifecycleError> {
        // A log that cannot be removed must not hold the teardown back; the removal still stops
        // every earlier attempt from writing to it.
        if let Err(error) = self
            .preparation_logs
            .remove(PreparationOwner::Model, &deployment.model_deployment_id)
        {
            warn!(
                event_kind = "preparation_log_removal_failed",
                deployment_id = %deployment.model_deployment_id,
                error = %error,
                "Preparation logs of a cleaned-up model deployment could not be removed"
            );
        }
        self.state
            .confirm_node_model_cleanup(
                &deployment.model_deployment_id,
                deployment.lifecycle_generation,
                realization_generation,
            )
            .map_err(ModelLifecycleError::from_current_state)?;
        // Cleanup is confirmed only once the deployment's workloads are gone.
        self.workload_lifecycle
            .removed(&deployment.model_deployment_id);
        Ok(())
    }

    fn cleanup_removes_current_state(&self) -> bool {
        true
    }

    fn cleanup_confirmed(
        &self,
        deployment: &ModelDeploymentDeclaration,
    ) -> Result<bool, ModelLifecycleError> {
        Ok(self
            .state
            .node_model_cleanup_evidence()
            .map_err(ModelLifecycleError::from_current_state)?
            .iter()
            .any(|evidence| {
                evidence.model_deployment_id == deployment.model_deployment_id
                    && evidence.lifecycle_generation == deployment.lifecycle_generation
            }))
    }
}
