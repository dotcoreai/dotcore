//! Execution-target keyed dispatch over infrastructure-neutral individual-workload adapters.

use std::sync::Arc;

use dotcore_domain::ExecutionPlatform;
use dotcore_runtime::{
    ObservedWorkload, ProcessMemoryObservation, RuntimeBackend, RuntimeBackendError,
    RuntimeBackendId, RuntimeFuture, RuntimeLogHistory, RuntimeLogStream,
    RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadCorrelation, WorkloadId,
    WorkloadLaunch, WorkloadMaterializationRequirements, WorkloadObservation,
    WorkloadStartupObserver, WorkloadStartupRequest, WorkloadStorageKey,
};
use dotcore_site::view::fencing::{
    AcceptedNodeViewAuthority, ActionPermitError, LifecycleOperationAttempt,
    LifecycleOperationToken, RuntimeActionAuthority, RuntimeActionPermit,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, debug_span, error, field, info, info_span};

use super::{
    Admission, dispatch_admitted,
    registry::{KeyedRegistry, KeyedRegistryError},
};
use crate::management_logs::ProtectedLogValues;

/// One observation plus the execution target whose adapter returned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetObservation {
    pub execution_target_id: String,

    pub observation: WorkloadObservation,
}

/// A complete model execution-target map with no default runtime adapter.
///
/// An execution target identifies exact SiteNode/target pairs selected by the inference controller. This map
/// dispatches only to the exact configured adapter; it does not schedule, load balance, or fall
/// back between execution targets.
#[derive(Clone)]
pub struct RuntimeTargetMap {
    adapters: KeyedRegistry<dyn RuntimeBackend>,
    action_authority: RuntimeActionAuthority,
}

impl RuntimeTargetMap {
    /// Returns the configured execution-target IDs in dispatch order.
    pub fn target_ids(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys()
    }

    /// Reports whether this controller runs the execution target. Each process holds runtime
    /// backends only for targets on its bound SiteNode, so this also means "on this node".
    pub fn runs_target(&self, execution_target_id: &str) -> bool {
        self.adapters.get(execution_target_id).is_some()
    }

    pub(crate) fn capture_operation(
        &self,
        deployment: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        attempt: LifecycleOperationAttempt,
    ) -> Result<LifecycleOperationToken, RuntimeTargetError> {
        self.action_authority
            .capture_operation(deployment, attempt)
            .map_err(|_| RuntimeTargetError::ActionRejected)
    }

    pub(crate) fn commit_operation<T>(
        &self,
        token: &LifecycleOperationToken,
        commit: impl FnOnce() -> T,
    ) -> Result<T, RuntimeTargetError> {
        self.action_authority
            .commit_operation(token, commit)
            .map_err(commit_fencing_error)
    }

    pub(crate) fn authorize_deployment_action(
        &self,
        deployment: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    ) -> Result<RuntimeActionPermit, RuntimeTargetError> {
        self.action_authority
            .authorize_deployment(deployment)
            .map_err(|_| RuntimeTargetError::ActionRejected)
    }

    /// Returns the backend identity for an execution target without fallback dispatch.
    pub fn backend_id(
        &self,
        execution_target_id: &str,
    ) -> Result<RuntimeBackendId, RuntimeTargetError> {
        Ok(RuntimeBackend::backend_id(
            &**self.adapter(execution_target_id)?,
        ))
    }

    /// Reports the effective execution environment for one exact execution target.
    pub async fn execution_platform(
        &self,
        execution_target_id: &str,
    ) -> Result<ExecutionPlatform, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_adapter_action(
            execution_target_id,
            debug_span!(
                "runtime_execution_platform",
                operation = "runtime_execution_platform",
                execution_target_id = %execution_target_id,
                outcome = field::Empty,
                error_kind = field::Empty,
            ),
            "execution_platform",
            adapter.execution_platform(),
        )
        .await
    }

    /// Returns the launch-asset requirements declared by one exact runtime adapter.
    pub fn materialization_requirements(
        &self,
        execution_target_id: &str,
    ) -> Result<WorkloadMaterializationRequirements, RuntimeTargetError> {
        Ok(self
            .adapter(execution_target_id)?
            .materialization_requirements())
    }

    /// Runs runtime-owned preparation for one exact admitted realization.
    pub async fn prepare_realization(
        &self,
        execution_target_id: &str,
        request: &WorkloadStartupRequest,
        cancellation: CancellationToken,
        observer: Option<WorkloadStartupObserver>,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_adapter_action(
            execution_target_id,
            debug_span!(
                "runtime_prepare_realization",
                operation = "runtime_prepare_realization",
                execution_target_id = %execution_target_id,
                outcome = field::Empty,
                error_kind = field::Empty,
            ),
            "prepare_realization",
            adapter.prepare_realization(request, cancellation, observer),
        )
        .await
    }

    /// Discovers workloads from every configured target and reports the target on each observation.
    pub async fn discover_workloads(&self) -> Result<Vec<TargetObservation>, RuntimeTargetError> {
        let mut result = Vec::new();
        for (target_id, adapter) in self.adapters.iter() {
            let span = debug_span!(
                "runtime_discover_workloads",
                operation = "runtime_discover_workloads",
                execution_target_id = %target_id,
                observation_count = field::Empty,
                outcome = field::Empty,
                error_kind = field::Empty,
            );
            let observations = dispatch_adapter_action(
                target_id,
                span.clone(),
                "discover_workloads",
                adapter.discover_workloads(),
            )
            .await;
            if let Ok(observations) = &observations {
                span.record("observation_count", observations.len());
            }
            result.extend(
                observations?
                    .into_iter()
                    .map(|observation| TargetObservation {
                        execution_target_id: target_id.clone(),
                        observation,
                    }),
            );
        }
        Ok(result)
    }

    /// Creates a workload through the adapter selected by `execution_target_id`.
    #[cfg(test)]
    pub async fn create_workload(
        &self,
        execution_target_id: &str,
        workload: &WorkloadLaunch,
    ) -> Result<WorkloadObservation, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        let span = info_span!(
            "runtime_create_workload",
            operation = "runtime_create_workload",
            execution_target_id = %execution_target_id,
            owner_id = %workload.correlation.owner_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let capability = workload
            .validate_backend_capabilities(&**adapter)
            .map_err(|source| RuntimeTargetError::Adapter {
                execution_target_id: execution_target_id.into(),
                source,
            });
        if let Err(error) = capability {
            let result = Err(error);
            record_runtime_result(
                &span,
                &result,
                "create_workload",
                &ProtectedLogValues::default(),
            );
            return result;
        }
        let _permit = self
            .action_authority
            .authorize()
            .map_err(|_| RuntimeTargetError::ActionRejected)?;
        dispatch_adapter_action(
            execution_target_id,
            span,
            "create_workload",
            adapter.create_workload(workload),
        )
        .await
    }

    /// Creates a workload under an admitted lifecycle operation; a failure's log event never
    /// shows the `protected` values its runtime error may quote.
    pub(crate) async fn create_workload_admitted(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        workload: &WorkloadLaunch,
        protected: &ProtectedLogValues,
    ) -> Result<WorkloadObservation, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        workload
            .validate_backend_capabilities(&**adapter)
            .map_err(|source| RuntimeTargetError::Adapter {
                execution_target_id: execution_target_id.into(),
                source,
            })?;
        dispatch_admitted(
            &self.action_authority,
            Admission::Operation(token),
            commit_fencing_error,
            || {
                dispatch_protected_action(
                    execution_target_id,
                    info_span!(
                        "runtime_create_workload",
                        operation = "runtime_create_workload",
                        execution_target_id = %execution_target_id,
                        owner_id = %workload.correlation.owner_id,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "create_workload",
                    adapter.create_workload(workload),
                    protected,
                )
            },
        )
        .await
    }

    /// Starts a workload under an admitted lifecycle operation; a failure's log event never
    /// shows the `protected` values its runtime error may quote.
    pub(crate) async fn start_workload_admitted(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        workload_id: &WorkloadId,
        protected: &ProtectedLogValues,
    ) -> Result<WorkloadObservation, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_admitted(
            &self.action_authority,
            Admission::Operation(token),
            commit_fencing_error,
            || {
                dispatch_protected_action(
                    execution_target_id,
                    info_span!(
                        "runtime_start_workload",
                        operation = "runtime_start_workload",
                        execution_target_id = %execution_target_id,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "start_workload",
                    adapter.start_workload(workload_id),
                    protected,
                )
            },
        )
        .await
    }

    /// Inspects a workload through the adapter selected by `execution_target_id`.
    pub async fn inspect_workload(
        &self,
        execution_target_id: &str,
        workload_id: &WorkloadId,
    ) -> Result<WorkloadObservation, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        let span = debug_span!(
            "runtime_inspect_workload",
            operation = "runtime_inspect_workload",
            execution_target_id = %execution_target_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        dispatch_adapter_action(
            execution_target_id,
            span,
            "inspect_workload",
            adapter.inspect_workload(workload_id),
        )
        .await
    }

    /// Measures a workload's process memory through the adapter selected by `execution_target_id`.
    pub async fn observe_process_memory(
        &self,
        execution_target_id: &str,
        workload_id: &WorkloadId,
    ) -> Result<ProcessMemoryObservation, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_log_action(
            execution_target_id,
            debug_span!("runtime_observe_process_memory", execution_target_id = %execution_target_id),
            adapter.observe_process_memory(workload_id),
        )
        .await
    }

    /// Stops a workload through the adapter selected by `execution_target_id`.
    #[cfg(test)]
    pub async fn stop_workload(
        &self,
        execution_target_id: &str,
        workload_id: &WorkloadId,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        let span = info_span!(
            "runtime_stop_workload",
            operation = "runtime_stop_workload",
            execution_target_id = %execution_target_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let _permit = self
            .action_authority
            .authorize()
            .map_err(|_| RuntimeTargetError::ActionRejected)?;
        dispatch_adapter_action(
            execution_target_id,
            span,
            "stop_workload",
            adapter.stop_workload(workload_id),
        )
        .await
    }

    pub(crate) async fn stop_workload_admitted(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        workload_id: &WorkloadId,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_admitted(
            &self.action_authority,
            Admission::Operation(token),
            commit_fencing_error,
            || {
                dispatch_adapter_action(
                    execution_target_id,
                    info_span!(
                        "runtime_stop_workload",
                        operation = "runtime_stop_workload",
                        execution_target_id = %execution_target_id,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "stop_workload",
                    adapter.stop_workload(workload_id),
                )
            },
        )
        .await
    }

    /// Removes a workload through the adapter selected by `execution_target_id`.
    #[cfg(test)]
    pub async fn remove_workload(
        &self,
        execution_target_id: &str,
        workload_id: &WorkloadId,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        let span = info_span!(
            "runtime_remove_workload",
            operation = "runtime_remove_workload",
            execution_target_id = %execution_target_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let _permit = self
            .action_authority
            .authorize()
            .map_err(|_| RuntimeTargetError::ActionRejected)?;
        dispatch_adapter_action(
            execution_target_id,
            span,
            "remove_workload",
            adapter.remove_workload(workload_id),
        )
        .await
    }

    pub(crate) async fn remove_workload_admitted(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        workload_id: &WorkloadId,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_admitted(
            &self.action_authority,
            Admission::Operation(token),
            commit_fencing_error,
            || {
                dispatch_adapter_action(
                    execution_target_id,
                    info_span!(
                        "runtime_remove_workload",
                        operation = "runtime_remove_workload",
                        execution_target_id = %execution_target_id,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "remove_workload",
                    adapter.remove_workload(workload_id),
                )
            },
        )
        .await
    }

    /// Removes one owner key's storage through only the runtime configured for
    /// `execution_target_id`, under the operation's lifecycle fence; absent storage is already
    /// removed.
    pub(crate) async fn remove_storage_admitted(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        storage: &WorkloadStorageKey,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_admitted(
            &self.action_authority,
            Admission::Operation(token),
            commit_fencing_error,
            || {
                dispatch_adapter_action(
                    execution_target_id,
                    info_span!(
                        "runtime_remove_storage",
                        operation = "runtime_remove_storage",
                        execution_target_id = %execution_target_id,
                        namespace = %storage.namespace,
                        key = %storage.key,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "remove_storage",
                    adapter.remove_storage(storage),
                )
            },
        )
        .await
    }

    /// Stops an owned workload whose deployment is absent from the controller's intent. The
    /// action starts only while no accepted deployment owns `workload`.
    pub(crate) async fn stop_orphan_workload(
        &self,
        execution_target_id: &str,
        workload: &ObservedWorkload,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        let admission = Admission::Orphan {
            owner: dotcore_domain::deployment::DeploymentOwner::Model,
            owner_id: &workload.correlation.owner_id,
        };
        dispatch_admitted(
            &self.action_authority,
            admission,
            commit_fencing_error,
            || {
                dispatch_adapter_action(
                    execution_target_id,
                    info_span!(
                        "runtime_stop_workload",
                        operation = "runtime_stop_workload",
                        execution_target_id = %execution_target_id,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "stop_workload",
                    adapter.stop_workload(&workload.workload_id),
                )
            },
        )
        .await
    }

    /// Removes an owned workload whose deployment is absent from the controller's intent. The
    /// action starts only while no accepted deployment owns `workload`.
    pub(crate) async fn remove_orphan_workload(
        &self,
        execution_target_id: &str,
        workload: &ObservedWorkload,
    ) -> Result<(), RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        let admission = Admission::Orphan {
            owner: dotcore_domain::deployment::DeploymentOwner::Model,
            owner_id: &workload.correlation.owner_id,
        };
        dispatch_admitted(
            &self.action_authority,
            admission,
            commit_fencing_error,
            || {
                dispatch_adapter_action(
                    execution_target_id,
                    info_span!(
                        "runtime_remove_workload",
                        operation = "runtime_remove_workload",
                        execution_target_id = %execution_target_id,
                        outcome = field::Empty,
                        error_kind = field::Empty,
                    ),
                    "remove_workload",
                    adapter.remove_workload(&workload.workload_id),
                )
            },
        )
        .await
    }

    /// Reads bounded diagnostics through only the exact configured execution-target runtime.
    pub async fn read_workload_logs(
        &self,
        execution_target_id: &str,
        workload_id: &WorkloadId,
        query: &RuntimeWorkloadLogQuery,
    ) -> Result<RuntimeLogHistory, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_log_action(
            execution_target_id,
            debug_span!("runtime_read_workload_logs", execution_target_id = %execution_target_id),
            adapter.read_workload_logs(workload_id, query),
        )
        .await
    }

    /// Reads diagnostics by full owner correlation in only the exact selected target.
    pub async fn read_workload_logs_by_correlation(
        &self,
        execution_target_id: &str,
        correlation: &WorkloadCorrelation,
        query: &RuntimeWorkloadLogQuery,
    ) -> Result<RuntimeLogHistory, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_log_action(
            execution_target_id,
            debug_span!("runtime_read_workload_logs_by_correlation", execution_target_id = %execution_target_id),
            adapter.read_workload_logs_by_correlation(correlation, query),
        )
        .await
    }

    /// Follows diagnostics by full owner correlation in only the exact selected target.
    pub async fn follow_workload_logs_by_correlation(
        &self,
        execution_target_id: &str,
        correlation: &WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> Result<RuntimeLogStream, RuntimeTargetError> {
        let adapter = self.adapter(execution_target_id)?;
        dispatch_log_action(
            execution_target_id,
            debug_span!("runtime_follow_workload_logs_by_correlation", execution_target_id = %execution_target_id),
            adapter.follow_workload_logs_by_correlation(correlation, query),
        )
        .await
    }

    fn adapter(
        &self,
        execution_target_id: &str,
    ) -> Result<&Arc<dyn RuntimeBackend>, RuntimeTargetError> {
        self.adapters
            .get(execution_target_id)
            .ok_or_else(|| RuntimeTargetError::MissingTarget(execution_target_id.to_owned()))
    }
}

/// Builds a runtime map before it can be injected into a controller.
pub struct RuntimeTargetMapBuilder {
    adapters: KeyedRegistry<dyn RuntimeBackend>,
    action_authority: RuntimeActionAuthority,
}

impl RuntimeTargetMapBuilder {
    /// Starts a node's map, whose every runtime action is fenced by the lease of the node's
    /// accepted view.
    pub(crate) fn leased(view: AcceptedNodeViewAuthority) -> Self {
        Self {
            adapters: KeyedRegistry::new(),
            action_authority: RuntimeActionAuthority::Leased(view),
        }
    }

    /// Starts a map whose runtime actions are not fenced, for tests of runtime dispatch alone.
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            adapters: KeyedRegistry::new(),
            action_authority: RuntimeActionAuthority::LocalAuthoritative,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_action_authority(
        mut self,
        action_authority: RuntimeActionAuthority,
    ) -> Self {
        self.action_authority = action_authority;
        self
    }

    #[cfg(test)]
    pub fn insert(
        &mut self,
        execution_target_id: impl Into<String>,
        adapter: impl RuntimeBackend + 'static,
    ) -> Result<(), RuntimeTargetError> {
        self.insert_boxed(execution_target_id, Box::new(adapter))
    }

    pub(crate) fn insert_boxed(
        &mut self,
        execution_target_id: impl Into<String>,
        adapter: Box<dyn RuntimeBackend>,
    ) -> Result<(), RuntimeTargetError> {
        self.adapters
            .insert(execution_target_id, Arc::from(adapter))
            .map_err(|error| match error {
                KeyedRegistryError::InvalidKey => RuntimeTargetError::InvalidTarget,
                KeyedRegistryError::DuplicateKey(key) => RuntimeTargetError::DuplicateTarget(key),
            })
    }

    #[cfg(test)]
    pub fn build(self) -> Result<RuntimeTargetMap, RuntimeTargetError> {
        if self.adapters.is_empty() {
            return Err(RuntimeTargetError::Empty);
        }
        Ok(RuntimeTargetMap {
            adapters: self.adapters,
            action_authority: self.action_authority,
        })
    }

    /// Builds an empty-capable map for a site that has no model execution targets.
    pub fn build_allow_empty(self) -> RuntimeTargetMap {
        RuntimeTargetMap {
            adapters: self.adapters,
            action_authority: self.action_authority,
        }
    }
}

#[cfg(test)]
impl Default for RuntimeTargetMapBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn commit_fencing_error(error: ActionPermitError) -> RuntimeTargetError {
    match error {
        ActionPermitError::StaleOperation => RuntimeTargetError::StaleCompletion,
        _ => RuntimeTargetError::ActionRejected,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeTargetError {
    #[error("Runtime execution target map is empty")]
    Empty,

    #[error("Runtime execution target ID is invalid")]
    InvalidTarget,

    #[error("execution target {0:?} has duplicate runtime adapters")]
    DuplicateTarget(String),

    #[error("execution target {0:?} has no runtime adapter")]
    MissingTarget(String),

    #[error("Runtime mutation was rejected by lifecycle fencing")]
    ActionRejected,

    #[error("Runtime completion belongs to a replaced lifecycle session")]
    StaleCompletion,

    #[error("execution target {execution_target_id:?} runtime adapter failure: {source}")]
    Adapter {
        execution_target_id: Box<str>,
        #[source]
        source: RuntimeBackendError,
    },
}

async fn dispatch_adapter_action<T>(
    execution_target_id: &str,
    span: Span,
    operation: &'static str,
    action: RuntimeFuture<'_, T>,
) -> Result<T, RuntimeTargetError> {
    dispatch_protected_action(
        execution_target_id,
        span,
        operation,
        action,
        &ProtectedLogValues::default(),
    )
    .await
}

/// Runs an adapter action like [`dispatch_adapter_action`], redacting `protected` from the
/// logged failure.
async fn dispatch_protected_action<T>(
    execution_target_id: &str,
    span: Span,
    operation: &'static str,
    action: RuntimeFuture<'_, T>,
    protected: &ProtectedLogValues,
) -> Result<T, RuntimeTargetError> {
    let result =
        action
            .instrument(span.clone())
            .await
            .map_err(|source| RuntimeTargetError::Adapter {
                execution_target_id: execution_target_id.into(),
                source,
            });
    record_runtime_result(&span, &result, operation, protected);
    result
}

async fn dispatch_log_action<T>(
    execution_target_id: &str,
    span: Span,
    action: RuntimeFuture<'_, T>,
) -> Result<T, RuntimeTargetError> {
    action
        .instrument(span)
        .await
        .map_err(|source| RuntimeTargetError::Adapter {
            execution_target_id: execution_target_id.into(),
            source,
        })
}

fn record_runtime_result<T>(
    span: &Span,
    result: &Result<T, RuntimeTargetError>,
    operation: &'static str,
    protected: &ProtectedLogValues,
) {
    match result {
        Ok(_) => {
            span.record("outcome", "success");
            if matches!(
                operation,
                "create_workload" | "start_workload" | "stop_workload" | "remove_workload"
            ) {
                info!(parent: span, event_kind = "model_runtime_outcome", operation, outcome = "success", "Model runtime operation completed");
            } else {
                debug!(parent: span, event_kind = "model_runtime_inspection", operation, outcome = "success", "Model runtime operation completed");
            }
        }
        Err(error) => {
            span.record("outcome", "failed");
            span.record("error_kind", "adapter");
            error!(parent: span, event_kind = "model_runtime_failure", operation, error_kind = "adapter", error = %protected.redact(&format!("{error:?}")), "Runtime operation failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::ready, sync::Arc};

    use anyhow::anyhow;
    use dotcore_domain::test_support::tracing::{Record, capture_records_async};
    use dotcore_runtime::{RuntimeBackend, RuntimeBackendError, RuntimeFuture};
    use dotcore_site::view::fencing::{
        AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
        RuntimeActionAuthority,
    };
    use tokio::sync::Notify;

    use super::*;

    struct TestRuntime {
        id: RuntimeBackendId,
        discovery_failure: bool,
    }

    impl RuntimeBackend for TestRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            self.id.clone()
        }

        fn supports_host_shared_ipc(&self) -> bool {
            self.id.0 == "runtime-host-ipc"
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            if self.discovery_failure {
                Box::pin(ready(Err(test_runtime_error())))
            } else {
                Box::pin(ready(Ok(Vec::new())))
            }
        }

        fn create_workload<'a>(
            &'a self,
            workload: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            assert!(
                self.id.0 != "runtime-no-host-ipc"
                    || workload.spec.ipc_mode != dotcore_runtime::WorkloadIpcMode::HostShared,
                "unsupported backend creation must not be called"
            );
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn start_workload<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn inspect_workload<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn remove_storage<'a>(
            &'a self,
            _: &'a dotcore_runtime::WorkloadStorageKey,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn read_workload_logs<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
            _query: &'a RuntimeWorkloadLogQuery,
        ) -> dotcore_runtime::RuntimeLogFuture<'a, RuntimeLogHistory> {
            Box::pin(ready(Ok(RuntimeLogHistory {
                entries: vec![dotcore_runtime::RuntimeLogEntry {
                    source: dotcore_runtime::RuntimeLogSource {
                        name: self.id.0.clone(),
                        label: self.id.0.clone(),
                    },
                    timestamp: std::time::SystemTime::UNIX_EPOCH,
                    stream: dotcore_runtime::RuntimeLogStreamKind::Stdout,
                    content: b"exact".to_vec(),
                    truncated: false,
                }],
                has_older_output: false,
                candidate_budget_truncated: false,
            })))
        }
    }

    struct DelayedStopRuntime {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl RuntimeBackend for DelayedStopRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            RuntimeBackendId("delayed-stop".to_owned())
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            Box::pin(ready(Ok(Vec::new())))
        }

        fn create_workload<'a>(
            &'a self,
            _workload: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn start_workload<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn inspect_workload<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(async move {
                self.started.notify_one();
                self.release.notified().await;
                Err(test_runtime_error())
            })
        }

        fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn remove_storage<'a>(
            &'a self,
            _: &'a dotcore_runtime::WorkloadStorageKey,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    fn test_runtime_error() -> RuntimeBackendError {
        RuntimeBackendError::InvalidLaunch(anyhow!("unused test operation"))
    }

    fn accepted_action_authority(lease_duration: std::time::Duration) -> RuntimeActionAuthority {
        accepted_action_authority_for(
            lease_duration,
            NodeViewProjection::new(Vec::new(), Vec::new()).expect("empty node view"),
        )
    }

    fn accepted_action_authority_for(
        lease_duration: std::time::Duration,
        projection: NodeViewProjection,
    ) -> RuntimeActionAuthority {
        let manager = ManagerSession::parse("a".repeat(32)).expect("manager session");
        let worker = NodeIncarnation::parse("b".repeat(32)).expect("worker incarnation");
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-node",
            worker,
            manager,
            lease_duration,
        )
        .expect("accepted view authority");
        authority
            .install_for_test("site-one", "worker-node", 1, &projection)
            .expect("initial accepted view");
        RuntimeActionAuthority::Leased(authority)
    }

    fn host_shared_ipc_launch() -> WorkloadLaunch {
        WorkloadLaunch::new(
            "deployment",
            dotcore_runtime::ExecutionTarget {
                managed_node_id: dotcore_domain::SiteNodeId::parse("node")
                    .expect("valid test node identity"),
                accelerator_device_ids: Vec::new(),
            },
            dotcore_runtime::WorkloadSpec {
                executable: dotcore_runtime::WorkloadExecutable {
                    artifact: dotcore_runtime::WorkloadExecutableArtifact::OciImage(
                        "example.test/workload@sha256:abc".to_owned(),
                    ),
                    path: "serve".to_owned(),
                },
                required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
                arguments: Vec::new(),
                environment: std::collections::BTreeMap::new(),
                artifacts: Vec::new(),
                storage: Vec::new(),
                ports: Vec::new(),
                accelerator_device_ids: Vec::new(),
                required_accelerator_device_count: 0,
                ipc_mode: dotcore_runtime::WorkloadIpcMode::HostShared,
                expected_memory: Default::default(),
            },
        )
    }

    #[test]
    fn duplicate_target_insertion_keeps_the_original_adapter() {
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target",
                TestRuntime {
                    id: RuntimeBackendId("first".to_owned()),
                    discovery_failure: false,
                },
            )
            .expect("first runtime adapter");

        assert!(matches!(
            builder.insert(
                "target",
                TestRuntime {
                    id: RuntimeBackendId("second".to_owned()),
                    discovery_failure: false,
                },
            ),
            Err(RuntimeTargetError::DuplicateTarget(target)) if target == "target"
        ));

        let map = builder.build().expect("Runtime target map");
        assert_eq!(
            map.backend_id("target").expect("target backend"),
            RuntimeBackendId("first".to_owned())
        );
    }

    #[test]
    fn execution_target_map_rejects_invalid_keys_and_uses_exact_lookup_without_fallback() {
        let mut builder = RuntimeTargetMapBuilder::new();
        assert!(matches!(
            builder.insert(
                " ",
                TestRuntime {
                    id: RuntimeBackendId("invalid".to_owned()),
                    discovery_failure: false
                }
            ),
            Err(RuntimeTargetError::InvalidTarget)
        ));
        builder
            .insert(
                "target-a",
                TestRuntime {
                    id: RuntimeBackendId("runtime-a".to_owned()),
                    discovery_failure: false,
                },
            )
            .expect("first runtime adapter");
        builder
            .insert(
                "target-b",
                TestRuntime {
                    id: RuntimeBackendId("runtime-b".to_owned()),
                    discovery_failure: false,
                },
            )
            .expect("second runtime adapter");
        let map = builder.build().expect("Runtime target map");

        assert_eq!(
            map.target_ids().collect::<Vec<_>>(),
            ["target-a", "target-b"]
        );
        assert_eq!(
            map.backend_id("target-b").expect("exact adapter"),
            RuntimeBackendId("runtime-b".to_owned())
        );
        assert!(matches!(
            map.backend_id("unknown"),
            Err(RuntimeTargetError::MissingTarget(target)) if target == "unknown"
        ));
    }
    #[tokio::test(start_paused = true)]
    async fn leased_runtime_map_blocks_new_mutations_after_expiry() {
        let mut builder = RuntimeTargetMapBuilder::new()
            .with_action_authority(accepted_action_authority(std::time::Duration::from_secs(5)));
        builder
            .insert(
                "target",
                TestRuntime {
                    id: RuntimeBackendId("runtime".to_owned()),
                    discovery_failure: false,
                },
            )
            .expect("Runtime adapter");
        let runtime = builder.build().expect("Runtime target map");
        let workload = WorkloadId("workload".to_owned());

        assert!(matches!(
            runtime.stop_workload("target", &workload).await,
            Err(RuntimeTargetError::Adapter { .. })
        ));

        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        for result in [
            runtime.stop_workload("target", &workload).await,
            runtime.remove_workload("target", &workload).await,
        ] {
            assert!(matches!(result, Err(RuntimeTargetError::ActionRejected)));
        }
    }

    #[tokio::test]
    async fn leased_orphan_actions_start_only_while_no_accepted_deployment_owns_the_workload() {
        let projection = NodeViewProjection::new(
            vec![dotcore_proto::dotcore::site::v1::ExecutionTarget {
                execution_target_id: "target".to_owned(),
                site_node_id: "worker-node".to_owned(),
                ..Default::default()
            }],
            vec![
                dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
                    model_deployment_id: "declared".to_owned(),
                    placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                        site_node_id: "worker-node".to_owned(),
                        execution_target_id: "target".to_owned(),
                    }),
                    ..Default::default()
                },
            ],
        )
        .expect("node view");
        let mut builder = RuntimeTargetMapBuilder::new().with_action_authority(
            accepted_action_authority_for(std::time::Duration::from_secs(5), projection),
        );
        builder
            .insert(
                "target",
                TestRuntime {
                    id: RuntimeBackendId("runtime".to_owned()),
                    discovery_failure: false,
                },
            )
            .expect("Runtime adapter");
        let runtime = builder.build().expect("Runtime target map");
        let workload = |owner_id: &str| ObservedWorkload {
            workload_id: WorkloadId("workload".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: owner_id.to_owned(),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId("runtime".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        };

        for result in [
            runtime
                .stop_orphan_workload("target", &workload("declared"))
                .await,
            runtime
                .remove_orphan_workload("target", &workload("declared"))
                .await,
        ] {
            assert!(matches!(result, Err(RuntimeTargetError::ActionRejected)));
        }
        for result in [
            runtime
                .stop_orphan_workload("target", &workload("undeclared"))
                .await,
            runtime
                .remove_orphan_workload("target", &workload("undeclared"))
                .await,
        ] {
            assert!(matches!(result, Err(RuntimeTargetError::Adapter { .. })));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn mutation_issued_before_lease_expiry_may_finish_after_expiry() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut builder = RuntimeTargetMapBuilder::new()
            .with_action_authority(accepted_action_authority(std::time::Duration::from_secs(5)));
        builder
            .insert(
                "target",
                DelayedStopRuntime {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                },
            )
            .expect("Runtime adapter");
        let runtime = builder.build().expect("Runtime target map");
        let issued_runtime = runtime.clone();
        let issued = tokio::spawn(async move {
            issued_runtime
                .stop_workload("target", &WorkloadId("issued".to_owned()))
                .await
        });

        started.notified().await;
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        release.notify_one();
        assert!(matches!(
            issued.await.expect("issued runtime task"),
            Err(RuntimeTargetError::Adapter { .. })
        ));
        assert!(matches!(
            runtime
                .stop_workload("target", &WorkloadId("new".to_owned()))
                .await,
            Err(RuntimeTargetError::ActionRejected)
        ));
    }

    #[tokio::test]
    async fn host_shared_ipc_requires_the_exact_selected_backend_capability() {
        let mut builder = RuntimeTargetMapBuilder::new();
        for (target, id) in [
            ("unsupported", "runtime-no-host-ipc"),
            ("supported", "runtime-host-ipc"),
        ] {
            builder
                .insert(
                    target,
                    TestRuntime {
                        id: RuntimeBackendId(id.to_owned()),
                        discovery_failure: false,
                    },
                )
                .expect("Runtime adapter");
        }
        let runtime = builder.build().expect("Runtime target map");
        let workload = host_shared_ipc_launch();

        let unsupported = runtime
            .create_workload("unsupported", &workload)
            .await
            .expect_err("host-shared IPC must fail closed");
        assert!(matches!(
            unsupported,
            RuntimeTargetError::Adapter { execution_target_id, source }
                if execution_target_id.as_ref() == "unsupported"
                    && matches!(source, RuntimeBackendError::HostSharedIpcUnavailable)
        ));

        let mut isolated = workload.clone();
        isolated.spec.ipc_mode = dotcore_runtime::WorkloadIpcMode::Isolated;
        let isolated_attempt = runtime
            .create_workload("unsupported", &isolated)
            .await
            .expect_err("isolated request reaches selected backend");
        assert!(matches!(
            isolated_attempt,
            RuntimeTargetError::Adapter { execution_target_id, source }
                if execution_target_id.as_ref() == "unsupported"
                    && matches!(source, RuntimeBackendError::InvalidLaunch(_))
        ));

        let supported = runtime
            .create_workload("supported", &workload)
            .await
            .expect_err("supported backend test operation");
        assert!(matches!(
            supported,
            RuntimeTargetError::Adapter { execution_target_id, source }
                if execution_target_id.as_ref() == "supported"
                    && matches!(source, RuntimeBackendError::InvalidLaunch(_))
        ));
    }

    #[tokio::test]
    async fn log_history_uses_only_the_exact_selected_target() {
        let mut builder = RuntimeTargetMapBuilder::new();
        for (target, id) in [("target-a", "runtime-a"), ("target-b", "runtime-b")] {
            builder
                .insert(
                    target,
                    TestRuntime {
                        id: RuntimeBackendId(id.to_owned()),
                        discovery_failure: false,
                    },
                )
                .expect("Runtime adapter");
        }
        let runtime = builder.build().expect("Runtime target map");
        let query = RuntimeWorkloadLogQuery::new(
            dotcore_runtime::RuntimeLogSource {
                name: "model-primary".to_owned(),
                label: "Primary model".to_owned(),
            },
            dotcore_runtime::RuntimeLogQuery::new(
                std::num::NonZeroU16::new(1).expect("non-zero entry limit"),
                None,
            )
            .expect("bounded query"),
        );

        let history = runtime
            .read_workload_logs("target-b", &WorkloadId("workload".to_owned()), &query)
            .await
            .expect("selected target logs");

        assert_eq!(history.entries[0].source.name, "runtime-b");
        assert_eq!(query.source().label, "Primary model");
        assert!(matches!(
            runtime
                .read_workload_logs("missing", &WorkloadId("workload".to_owned()), &query)
                .await,
            Err(RuntimeTargetError::MissingTarget(target)) if target == "missing"
        ));
    }

    #[tokio::test]
    async fn simulated_log_capability_absence_emits_no_normal_diagnostic_event() {
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "simulated",
                dotcore_runtime_backends::simulated::InMemoryRuntimeBackend::new(
                    dotcore_domain::ExecutionPlatform::linux_amd64(),
                ),
            )
            .expect("Runtime adapter");
        let runtime = builder.build().expect("Runtime target map");
        let source = dotcore_runtime::RuntimeLogSource {
            name: "model".to_owned(),
            label: "Model".to_owned(),
        };
        let query = RuntimeWorkloadLogQuery::new(
            source,
            dotcore_runtime::RuntimeLogQuery::new(
                std::num::NonZeroU16::new(200).expect("non-zero entry limit"),
                None,
            )
            .expect("bounded query"),
        );

        let (_, records, _) = capture_records_async(|| async {
            runtime
                .read_workload_logs("simulated", &WorkloadId("workload".to_owned()), &query)
                .await
                .expect_err("Simulated logs unavailable");
        })
        .await;

        assert!(!records.iter().any(|record| matches!(
            record,
            Record::Event { level, .. } if matches!(level.as_str(), "INFO" | "WARN" | "ERROR")
        )));
    }

    #[tokio::test]
    async fn runtime_failure_records_a_failed_span_and_error_event() {
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target",
                TestRuntime {
                    id: RuntimeBackendId("runtime/test".to_owned()),
                    discovery_failure: false,
                },
            )
            .expect("Runtime adapter");
        let runtime = builder.build().expect("Runtime target map");

        let (_, records, output) = capture_records_async(|| async {
            runtime
                .stop_workload("target", &WorkloadId("workload".to_owned()))
                .await
                .expect_err("Runtime failure");
        })
        .await;

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "runtime_stop_workload"
                    && fields.get("outcome").is_some_and(|value| value.contains("failed"))
                    && fields.get("error_kind").is_some_and(|value| value.contains("adapter"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "ERROR"
                    && fields.get("error_kind").is_some_and(|value| value.contains("adapter"))
        )));
        assert!(output.contains("unused test operation"));
    }

    #[tokio::test]
    async fn runtime_discovery_failure_records_a_failed_span_and_error_event() {
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target",
                TestRuntime {
                    id: RuntimeBackendId("runtime/test".to_owned()),
                    discovery_failure: true,
                },
            )
            .expect("Runtime adapter");
        let runtime = builder.build().expect("Runtime target map");

        let (_, records, output) = capture_records_async(|| async {
            runtime
                .discover_workloads()
                .await
                .expect_err("Runtime discovery failure");
        })
        .await;

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "runtime_discover_workloads"
                    && fields.get("outcome").is_some_and(|value| value.contains("failed"))
                    && fields.get("error_kind").is_some_and(|value| value.contains("adapter"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "ERROR"
                    && fields.get("error_kind").is_some_and(|value| value.contains("adapter"))
        )));
        assert!(output.contains("unused test operation"));
    }

    #[test]
    fn runtime_target_errors_keep_backend_and_leaf_sources() {
        let backend = RuntimeBackendError::InvalidLaunch(anyhow::Error::new(
            std::io::Error::other("daemon unavailable"),
        ));
        let error = RuntimeTargetError::Adapter {
            execution_target_id: "target".into(),
            source: backend,
        };

        let backend_source = std::error::Error::source(&error).expect("Runtime backend source");
        assert_eq!(
            backend_source.to_string(),
            "Invalid runtime launch: daemon unavailable"
        );
        assert_eq!(
            std::error::Error::source(backend_source)
                .expect("Runtime leaf source")
                .to_string(),
            "daemon unavailable"
        );
    }

    #[test]
    fn leaf_anyhow_source_chain_is_retained_through_typed_runtime_layers() {
        let backend = RuntimeBackendError::docker_with_source(
            "Docker client connection failed",
            std::io::Error::other("daemon unavailable"),
        );
        let error = RuntimeTargetError::Adapter {
            execution_target_id: "target".into(),
            source: backend,
        };

        let RuntimeTargetError::Adapter {
            source: backend, ..
        } = &error
        else {
            unreachable!("constructed runtime target adapter failure");
        };
        let RuntimeBackendError::Docker(leaf) = backend else {
            unreachable!("constructed Docker runtime failure");
        };
        let contextual = std::error::Error::source(backend).expect("contextual anyhow source");
        assert_eq!(contextual.to_string(), "Docker client connection failed");
        assert_eq!(
            contextual
                .source()
                .expect("Docker client leaf source")
                .to_string(),
            "daemon unavailable"
        );
        assert!(matches!(
            std::error::Error::source(&error),
            Some(source) if source.is::<RuntimeBackendError>()
        ));
        assert!(leaf.chain().any(|source| source.is::<std::io::Error>()));
    }
}
