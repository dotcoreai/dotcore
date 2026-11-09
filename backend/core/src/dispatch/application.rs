//! Execution-target-keyed application workload-group dispatch without remote fallback.
//!
//! Every runtime mutation names the captured lifecycle operation it acts for and starts only while
//! that operation is still admitted, so a worker whose view lease expired keeps its groups running
//! but starts no new action. Reads and inspections are never fenced.

use std::{collections::BTreeMap, sync::Arc};

use dotcore_domain::valid_id;
use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeBackendError, RuntimeBackendId, RuntimeGroupLogFollowQuery,
    RuntimeLogQuery, RuntimeLogSourceHistory, RuntimeLogSourceSelection, RuntimeLogSourceStream,
    WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupLaunch, WorkloadGroupObservation,
    WorkloadMaterializationRequirements, WorkloadStorageKey, WorkloadUnitName,
    WorkloadUnitResourceObservation,
};
use dotcore_site::view::fencing::{
    AcceptedNodeViewAuthority, ActionAssignment, ActionPermitError, LifecycleOperationAttempt,
    LifecycleOperationToken, RuntimeActionAuthority,
};
use tracing::{debug, error, info};

use super::{Admission, dispatch_admitted};

/// One workload-group observation together with its configured execution-target identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetWorkloadGroupObservation {
    pub execution_target_id: String,
    pub observation: WorkloadGroupObservation,
}

/// A complete execution-target-keyed map of application runtime adapters.
///
/// Site composition supplies only entries owned by the locally bound node. The keyed shape provides
/// exact dispatch across any configured local targets without authorizing remote dispatch. The map is
/// immutable once built, so cloning shares the same entries without copying adapters.
#[derive(Clone)]
pub struct ApplicationRuntimeMap {
    target_adapters: Arc<BTreeMap<String, Arc<dyn WorkloadGroupBackend>>>,
    action_authority: RuntimeActionAuthority,
}

impl ApplicationRuntimeMap {
    /// Captures the exact accepted assignment one deployment's runtime actions act for. On a
    /// worker the capture is refused while the view lease is expired or no longer assigns it.
    pub(crate) fn capture_operation(
        &self,
        assignment: &ActionAssignment,
        attempt: LifecycleOperationAttempt,
    ) -> Result<LifecycleOperationToken, ApplicationRuntimeError> {
        self.action_authority
            .capture_operation(assignment.clone(), attempt)
            .map_err(fenced)
    }

    /// Runs one synchronous owner state write for the operation `token` captured, only while that
    /// operation is still admitted, so a write made part-way through it never outlives its lease.
    pub(crate) fn commit_operation<T>(
        &self,
        token: &LifecycleOperationToken,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ApplicationRuntimeError> {
        self.action_authority
            .commit_operation(token, commit)
            .map_err(fenced)
    }

    /// Returns configured execution-target identities in deterministic order.
    #[cfg(test)]
    pub fn target_ids(&self) -> impl Iterator<Item = &str> {
        self.target_adapters.keys().map(String::as_str)
    }

    /// Returns the runtime identity selected for one exact execution target.
    pub fn backend_id(
        &self,
        execution_target_id: &str,
    ) -> Result<RuntimeBackendId, ApplicationRuntimeError> {
        Ok(self.target_adapter(execution_target_id)?.backend_id())
    }

    /// Returns which launch assets the runtime of one exact execution target needs on the host.
    pub fn materialization_requirements(
        &self,
        execution_target_id: &str,
    ) -> Result<WorkloadMaterializationRequirements, ApplicationRuntimeError> {
        Ok(self
            .target_adapter(execution_target_id)?
            .materialization_requirements())
    }

    /// Discovers workload groups from every configured local application runtime.
    pub async fn discover_workload_groups(
        &self,
    ) -> Result<Vec<TargetWorkloadGroupObservation>, ApplicationRuntimeError> {
        let mut observations = Vec::new();
        for (execution_target_id, adapter) in self.target_adapters.iter() {
            let groups = adapter.discover_workload_groups().await.map_err(|source| {
                error!(event_kind = "application_runtime_discovery_failed", execution_target_id = %execution_target_id, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime workload-group discovery failed");
                ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.clone(),
                    source,
                }
            })?;
            debug!(event_kind = "application_runtime_discovery", execution_target_id = %execution_target_id, workload_group_count = groups.len(), "Application runtime workload groups inspected");
            observations.extend(groups.into_iter().map(|observation| {
                TargetWorkloadGroupObservation {
                    execution_target_id: execution_target_id.clone(),
                    observation,
                }
            }));
        }
        Ok(observations)
    }

    /// Creates a workload group through the runtime configured for `execution_target_id`.
    pub(crate) async fn create_workload_group(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<WorkloadGroupObservation, ApplicationRuntimeError> {
        self.validate_launch_target(execution_target_id, group)?;
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || adapter.create_workload_group(group))
            .await?;
        match result {
            Ok(observation) => {
                info!(event_kind = "application_runtime_create", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "created", "Application runtime workload group created");
                Ok(observation)
            }
            Err(source) => {
                error!(event_kind = "application_runtime_create", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime workload-group creation failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Starts retained workload-group units through the runtime configured for `execution_target_id`.
    pub(crate) async fn start_workload_group(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<WorkloadGroupObservation, ApplicationRuntimeError> {
        self.validate_launch_target(execution_target_id, group)?;
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || adapter.start_workload_group(group))
            .await?;
        match result {
            Ok(observation) => {
                info!(event_kind = "application_runtime_start", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "started", "Application runtime workload group started");
                Ok(observation)
            }
            Err(source) => {
                error!(event_kind = "application_runtime_start", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime workload-group start failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Checks retained storage through the runtime configured for the selected execution target.
    pub async fn storage_exists(
        &self,
        execution_target_id: &str,
        storage: &WorkloadStorageKey,
    ) -> Result<bool, ApplicationRuntimeError> {
        self.target_adapter(execution_target_id)?
            .storage_exists(storage)
            .await
            .map_err(|source| ApplicationRuntimeError::TargetAdapter {
                execution_target_id: execution_target_id.to_owned(),
                source,
            })
    }

    /// Removes and confirms absence through only the runtime configured for `execution_target_id`.
    pub(crate) async fn ensure_workload_group_absent(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        group_id: &WorkloadGroupId,
        owner_id: &str,
    ) -> Result<(), ApplicationRuntimeError> {
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || {
                adapter.ensure_workload_group_absent(group_id, owner_id)
            })
            .await?;
        match result {
            Ok(()) => {
                info!(event_kind = "application_runtime_cleanup", execution_target_id = %execution_target_id, deployment_id = %group_id.0, outcome = "absent", "Application runtime workload group confirmed absent");
                Ok(())
            }
            Err(source) => {
                error!(event_kind = "application_runtime_cleanup", execution_target_id = %execution_target_id, deployment_id = %group_id.0, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime workload-group cleanup failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Removes one owner key's retained storage for one deployment through only the runtime
    /// configured for `execution_target_id`; absent storage is already removed.
    pub(crate) async fn remove_storage(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        deployment_id: &str,
        storage: &WorkloadStorageKey,
    ) -> Result<(), ApplicationRuntimeError> {
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || adapter.remove_storage(storage))
            .await?;
        match result {
            Ok(()) => {
                info!(event_kind = "application_runtime_storage_removal", execution_target_id = %execution_target_id, deployment_id, namespace = %storage.namespace, key = %storage.key, outcome = "absent", "Application runtime storage confirmed absent");
                Ok(())
            }
            Err(source) => {
                error!(event_kind = "application_runtime_storage_removal", execution_target_id = %execution_target_id, deployment_id, namespace = %storage.namespace, key = %storage.key, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime storage removal failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Inspects a workload group through the runtime configured for `execution_target_id`.
    pub async fn inspect_workload_group(
        &self,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<WorkloadGroupObservation, ApplicationRuntimeError> {
        let result = self
            .target_adapter(execution_target_id)?
            .inspect_workload_group(group)
            .await;
        if let Ok(observation) = &result {
            debug!(event_kind = "application_runtime_inspection", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, state = observation_state(observation), unit_count = observation_unit_count(observation), outcome = "inspected", "Application runtime workload group inspected");
        }
        result.map_err(|source| ApplicationRuntimeError::TargetAdapter {
            execution_target_id: execution_target_id.to_owned(),
            source,
        })
    }

    /// Measures the CPU time and memory of each unit of a workload group through the runtime
    /// configured for `execution_target_id`.
    pub(crate) async fn observe_group_resources(
        &self,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>, ApplicationRuntimeError>
    {
        self.target_adapter(execution_target_id)?
            .observe_group_resources(group)
            .await
            .map_err(|source| ApplicationRuntimeError::TargetAdapter {
                execution_target_id: execution_target_id.to_owned(),
                source,
            })
    }

    /// Removes a workload group through the runtime configured for `execution_target_id`.
    pub(crate) async fn remove_workload_group(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<(), ApplicationRuntimeError> {
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || adapter.remove_workload_group(group))
            .await?;
        match result {
            Ok(()) => {
                info!(event_kind = "application_runtime_remove", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "removed", "Application runtime workload group removed");
                Ok(())
            }
            Err(source) => {
                error!(event_kind = "application_runtime_remove", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime workload-group removal failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Removes a previously observed exact-owned group through the runtime configured for `execution_target_id`.
    pub(crate) async fn remove_observed_workload_group(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        group: &ObservedWorkloadGroup,
    ) -> Result<(), ApplicationRuntimeError> {
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || adapter.remove_observed_workload_group(group))
            .await?;
        match result {
            Ok(()) => {
                info!(event_kind = "application_runtime_remove", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "removed_observed", "Application runtime observed workload group removed");
                Ok(())
            }
            Err(source) => {
                error!(event_kind = "application_runtime_remove", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime observed workload-group removal failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Stops retained workload-group units through the runtime configured for `execution_target_id`.
    pub(crate) async fn stop_workload_group(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<(), ApplicationRuntimeError> {
        let adapter = self.target_adapter(execution_target_id)?;
        let result = self
            .admitted(token, || adapter.stop_workload_group(group))
            .await?;
        match result {
            Ok(()) => {
                info!(event_kind = "application_runtime_stop", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "stopped", "Application runtime workload group stopped");
                Ok(())
            }
            Err(source) => {
                error!(event_kind = "application_runtime_stop", execution_target_id = %execution_target_id, deployment_id = %group.group_id.0, outcome = "failed", error_kind = runtime_error_kind(&source), "Application runtime workload-group stop failed");
                Err(ApplicationRuntimeError::TargetAdapter {
                    execution_target_id: execution_target_id.to_owned(),
                    source,
                })
            }
        }
    }

    /// Reads bounded diagnostics through only the exact configured execution-target runtime.
    pub async fn read_workload_group_logs(
        &self,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
        query: &RuntimeLogQuery,
    ) -> Result<Vec<RuntimeLogSourceHistory>, ApplicationRuntimeError> {
        self.target_adapter(execution_target_id)?
            .read_workload_group_logs(group, sources, query)
            .await
            .map_err(|source| ApplicationRuntimeError::TargetAdapter {
                execution_target_id: execution_target_id.to_owned(),
                source,
            })
    }

    /// Follows diagnostics through only the exact configured execution-target runtime.
    pub async fn follow_workload_group_logs(
        &self,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> Result<Vec<RuntimeLogSourceStream>, ApplicationRuntimeError> {
        self.target_adapter(execution_target_id)?
            .follow_workload_group_logs(group, sources, query)
            .await
            .map_err(|source| ApplicationRuntimeError::TargetAdapter {
                execution_target_id: execution_target_id.to_owned(),
                source,
            })
    }

    /// Starts one adapter action only while `token` is still admitted. The outer result reports
    /// the admission; the inner one is the adapter's own outcome.
    async fn admitted<'a, T>(
        &self,
        token: &LifecycleOperationToken,
        action: impl FnOnce() -> dotcore_runtime::RuntimeFuture<'a, T>,
    ) -> Result<Result<T, RuntimeBackendError>, ApplicationRuntimeError> {
        dispatch_admitted(
            &self.action_authority,
            Admission::Operation(token),
            fenced,
            || async { Ok(action().await) },
        )
        .await
    }

    fn validate_launch_target(
        &self,
        execution_target_id: &str,
        group: &WorkloadGroupLaunch,
    ) -> Result<(), ApplicationRuntimeError> {
        self.target_adapter(execution_target_id)?;
        if group.execution_target_id != execution_target_id {
            return Err(ApplicationRuntimeError::TargetAdapter {
                execution_target_id: execution_target_id.to_owned(),
                source: RuntimeBackendError::ExecutionTargetMismatch {
                    required_execution_target_id: group.execution_target_id.clone(),
                    observed_execution_target_id: execution_target_id.to_owned(),
                },
            });
        }
        Ok(())
    }

    fn target_adapter(
        &self,
        execution_target_id: &str,
    ) -> Result<&Arc<dyn WorkloadGroupBackend>, ApplicationRuntimeError> {
        self.target_adapters
            .get(execution_target_id)
            .ok_or_else(|| ApplicationRuntimeError::MissingTarget(execution_target_id.to_owned()))
    }
}

fn fenced(error: ActionPermitError) -> ApplicationRuntimeError {
    debug!(event_kind = "application_runtime_action_fenced", reason = %error, "Application runtime action was not admitted");
    ApplicationRuntimeError::ActionRejected
}

fn runtime_error_kind(error: &RuntimeBackendError) -> &'static str {
    match error {
        RuntimeBackendError::ExecutionPlatformUnavailable => "execution_platform_unavailable",
        RuntimeBackendError::ExecutionPlatform(_) => "execution_platform",
        RuntimeBackendError::TargetPlatformMismatch { .. } => "target_platform_mismatch",
        RuntimeBackendError::ExecutionTargetMismatch { .. } => "execution_target_mismatch",
        RuntimeBackendError::ImagePlatformMismatch { .. } => "image_platform_mismatch",
        RuntimeBackendError::Docker(_) => "docker",
        RuntimeBackendError::Native(_) => "native",
        RuntimeBackendError::ImageMissing(_) => "image_missing",
        RuntimeBackendError::PublishedPortInUse(_) => "published_port_in_use",
        RuntimeBackendError::InvalidLaunch(_) => "invalid_launch",
        RuntimeBackendError::UnsupportedExecutableArtifact { .. } => {
            "unsupported_executable_artifact"
        }
        RuntimeBackendError::Ownership(_) => "ownership",
        RuntimeBackendError::PreparationCancelled => "preparation_cancelled",
        RuntimeBackendError::HostSharedIpcUnavailable => "host_shared_ipc_unavailable",
        RuntimeBackendError::LogCapabilityUnavailable => "log_capability_unavailable",
        RuntimeBackendError::WorkloadNotCreated => "workload_not_created",
        RuntimeBackendError::LogCancelled => "log_cancelled",
        RuntimeBackendError::RemoteLogUnavailable => "remote_log_unavailable",
        RuntimeBackendError::LogSourceLimitReached => "log_source_limit_reached",
        RuntimeBackendError::Observation(_) => "observation",
    }
}

fn observation_state(observation: &WorkloadGroupObservation) -> &'static str {
    match observation {
        WorkloadGroupObservation::Observed(_) => "observed",
        WorkloadGroupObservation::StaleConfiguration { .. } => "stale_configuration",
        WorkloadGroupObservation::Absent { .. } => "absent",
        WorkloadGroupObservation::Unknown { .. } => "unknown",
    }
}

fn observation_unit_count(observation: &WorkloadGroupObservation) -> usize {
    match observation {
        WorkloadGroupObservation::Observed(group)
        | WorkloadGroupObservation::StaleConfiguration { group, .. } => group.units.len(),
        WorkloadGroupObservation::Absent { .. } | WorkloadGroupObservation::Unknown { .. } => 0,
    }
}

/// Builds an exact execution-target keyed application runtime map before controller injection.
pub struct ApplicationRuntimeMapBuilder {
    target_adapters: Arc<BTreeMap<String, Arc<dyn WorkloadGroupBackend>>>,
    action_authority: RuntimeActionAuthority,
}

impl ApplicationRuntimeMapBuilder {
    /// Starts a node's map, whose every runtime action is fenced by the lease of the node's
    /// accepted view.
    pub(crate) fn leased(view: AcceptedNodeViewAuthority) -> Self {
        Self {
            target_adapters: Arc::new(BTreeMap::new()),
            action_authority: RuntimeActionAuthority::Leased(view),
        }
    }

    /// Starts a map whose runtime actions are not fenced, for tests of runtime dispatch alone.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self {
            target_adapters: Arc::new(BTreeMap::new()),
            action_authority: RuntimeActionAuthority::LocalAuthoritative,
        }
    }

    #[cfg(test)]
    pub fn insert(
        &mut self,
        execution_target_id: impl Into<String>,
        adapter: impl WorkloadGroupBackend + 'static,
    ) -> Result<(), ApplicationRuntimeError> {
        self.insert_boxed(execution_target_id, Box::new(adapter))
    }

    pub(crate) fn insert_boxed(
        &mut self,
        execution_target_id: impl Into<String>,
        adapter: Box<dyn WorkloadGroupBackend>,
    ) -> Result<(), ApplicationRuntimeError> {
        let execution_target_id = execution_target_id.into();
        if !valid_id(&execution_target_id) {
            return Err(ApplicationRuntimeError::InvalidTarget);
        }
        if self.target_adapters.contains_key(&execution_target_id) {
            return Err(ApplicationRuntimeError::DuplicateTarget(
                execution_target_id,
            ));
        }
        Arc::make_mut(&mut self.target_adapters).insert(execution_target_id, Arc::from(adapter));
        Ok(())
    }

    pub fn build(self) -> ApplicationRuntimeMap {
        ApplicationRuntimeMap {
            target_adapters: self.target_adapters,
            action_authority: self.action_authority,
        }
    }
}

/// Errors from exact execution-target application runtime dispatch.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationRuntimeError {
    #[error("Application execution-target ID is invalid")]
    InvalidTarget,

    #[error("Application execution target {0:?} has duplicate runtime bindings")]
    DuplicateTarget(String),

    #[error("Application execution target {0:?} has no runtime binding")]
    MissingTarget(String),

    #[error("Application runtime action was refused because the node view lease does not admit it")]
    ActionRejected,

    #[error("Application execution target {execution_target_id:?} runtime failed: {source}")]
    TargetAdapter {
        execution_target_id: String,
        #[source]
        source: RuntimeBackendError,
    },
}

#[cfg(test)]
mod tests {
    use dotcore_domain::{ExecutionPlatform, SiteNodeId};
    use dotcore_runtime::{
        ExecutionTarget, WorkloadCorrelation, WorkloadGroupId, WorkloadGroupLaunch,
        WorkloadPublicationRequest, WorkloadUnitName,
    };
    use dotcore_runtime_backends::simulated::InMemoryRuntimeBackend;
    use dotcore_site::view::fencing::{
        AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
    };

    use super::*;

    #[test]
    fn builder_rejects_invalid_and_duplicate_exact_targets() {
        let mut builder = ApplicationRuntimeMapBuilder::new();
        assert!(matches!(
            builder.insert(
                " ",
                InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64())
            ),
            Err(ApplicationRuntimeError::InvalidTarget)
        ));
        builder
            .insert(
                "target-a",
                InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64()),
            )
            .expect("target-a");
        assert!(matches!(
            builder.insert(
                "target-a",
                InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64()),
            ),
            Err(ApplicationRuntimeError::DuplicateTarget(target)) if target == "target-a"
        ));
    }

    #[tokio::test]
    async fn exact_target_dispatch_never_falls_back_to_another_target() {
        let mut builder = ApplicationRuntimeMapBuilder::new();
        for target in ["target-a", "target-b"] {
            builder
                .insert(
                    target,
                    InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64()),
                )
                .expect("Runtime target");
        }
        let map = builder.build();
        let group = test_group("target-a");
        let token = local_token(&map);
        assert!(
            map.create_workload_group(&token, "target-a", &group)
                .await
                .is_ok()
        );
        assert!(matches!(
            map.inspect_workload_group("target-b", &group).await,
            Ok(WorkloadGroupObservation::Absent { .. })
        ));
        assert!(matches!(
            map.create_workload_group(&token, "missing", &group).await,
            Err(ApplicationRuntimeError::MissingTarget(target)) if target == "missing"
        ));
    }

    #[tokio::test]
    async fn launch_target_must_match_the_selected_dispatch_target() {
        let mut builder = ApplicationRuntimeMapBuilder::new();
        builder
            .insert(
                "target-a",
                InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64()),
            )
            .expect("Runtime target");
        let map = builder.build();
        let error = map
            .create_workload_group(&local_token(&map), "target-a", &test_group("target-b"))
            .await
            .expect_err("mismatched target");
        assert!(matches!(
            error,
            ApplicationRuntimeError::TargetAdapter {
                execution_target_id,
                source: RuntimeBackendError::ExecutionTargetMismatch { .. },
            } if execution_target_id == "target-a"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn leased_mutations_start_only_while_the_view_lease_admits_them() {
        let accepted = AcceptedNodeViewAuthority::new(
            "site-one",
            "node-one",
            NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("session"),
            std::time::Duration::from_secs(30),
        )
        .expect("view authority");
        let mut builder = ApplicationRuntimeMapBuilder::leased(accepted.clone());
        let runtime = InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64());
        builder
            .insert_boxed("target-a", Box::new(runtime))
            .expect("Runtime target");
        let map = builder.build();
        let assignment = assignment();
        let attempt = || LifecycleOperationAttempt::Runtime {
            reconcile_generation: 1,
        };
        assert!(matches!(
            map.capture_operation(&ActionAssignment::from(&assignment), attempt()),
            Err(ApplicationRuntimeError::ActionRejected)
        ));

        let projection = NodeViewProjection::new(
            vec![dotcore_proto::dotcore::site::v1::ExecutionTarget {
                execution_target_id: "target-a".to_owned(),
                site_node_id: "node-one".to_owned(),
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("projection")
        .with_application_assignments(vec![assignment.clone()])
        .expect("application projection");
        accepted
            .install_for_test("site-one", "node-one", 1, &projection)
            .expect("installed view");
        let token = map
            .capture_operation(&ActionAssignment::from(&assignment), attempt())
            .expect("admitted capture");
        let group = test_group("target-a");
        map.create_workload_group(&token, "target-a", &group)
            .await
            .expect("admitted creation");

        assert_eq!(
            map.commit_operation(&token, || 7).expect("admitted write"),
            7
        );

        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(matches!(
            map.stop_workload_group(&token, "target-a", &group).await,
            Err(ApplicationRuntimeError::ActionRejected)
        ));
        let mut written = false;
        assert!(matches!(
            map.commit_operation(&token, || written = true),
            Err(ApplicationRuntimeError::ActionRejected)
        ));
        assert!(!written, "a refused write never runs");
        assert!(
            matches!(
                map.inspect_workload_group("target-a", &group).await,
                Ok(WorkloadGroupObservation::Observed(observed))
                    if observed.units.values().all(|unit| unit.state == dotcore_runtime::WorkloadUnitState::Running)
            ),
            "an expired lease leaves the group running and still allows inspection"
        );
    }

    fn local_token(map: &ApplicationRuntimeMap) -> LifecycleOperationToken {
        map.capture_operation(
            &ActionAssignment::from(&assignment()),
            LifecycleOperationAttempt::Runtime {
                reconcile_generation: 1,
            },
        )
        .expect("local authority admits every operation")
    }

    fn assignment() -> dotcore_proto::dotcore::application::private::v1::ApplicationAssignment {
        dotcore_proto::dotcore::application::private::v1::ApplicationAssignment {
            declaration: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration {
                    application_deployment_id: "deployment-01".to_owned(),
                    application_deployment_name: "deployment-01".to_owned(),
                    placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                        site_node_id: "node-one".to_owned(),
                        execution_target_id: "target-a".to_owned(),
                    }),
                    lifecycle_generation: 1,
                    ..Default::default()
                },
            ),
            linked_route: None,
        }
    }

    fn test_group(execution_target_id: &str) -> WorkloadGroupLaunch {
        WorkloadGroupLaunch {
            group_id: WorkloadGroupId("application-01".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "deployment-01".to_owned(),
                reconcile_generation: 1,
            },
            execution_target: ExecutionTarget {
                managed_node_id: SiteNodeId::for_test("node-one"),
                accelerator_device_ids: Vec::new(),
            },
            execution_target_id: execution_target_id.to_owned(),
            required_platform: ExecutionPlatform::linux_amd64(),
            units: Default::default(),
            publication: WorkloadPublicationRequest {
                unit_name: WorkloadUnitName("web".to_owned()),
                port_name: "http".to_owned(),
                configured_host: std::net::Ipv4Addr::LOCALHOST,
                host_port: std::num::NonZeroU16::new(30_000).expect("nonzero port"),
            },
            host_aliases: Default::default(),
        }
    }
}
