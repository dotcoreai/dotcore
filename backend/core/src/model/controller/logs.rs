//! Serves read-only deployment logs for model deployments to the model
//! service, and to the site manager from the node that hosts them.
//!
//! The manager reads a deployment's logs from its hosting node, whatever its
//! lifecycle state and whether that node shares the manager process. The node
//! serves the output of each preparation step it ran for the deployment, then
//! reads or tails the runtime logs of the workload launch rebuilt from its
//! installed view and prepared artifacts, addressed by the launch's container
//! correlation. The configured upstream API keys and base URLs are also
//! returned so the caller can redact them from returned log lines.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use dotcore_artifact::ArtifactManager;
use dotcore_proto::dotcore::deployment::private::v1::DeploymentOwnerKind;
use dotcore_runtime::{
    RuntimeBackendError, RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceHistory,
    RuntimeLogSourceSelection, RuntimeLogSourceStream, RuntimeWorkloadLogFollowQuery,
    RuntimeWorkloadLogQuery, WorkloadCorrelation,
};
use dotcore_site::repository::SiteRepositoryError;

use super::{
    DesiredDeploymentRepository, InferenceController, ModelRealizationInput,
    launch::{resolved_launch, selected_target, workload_launch},
    state::is_absent,
};
use crate::{
    deployment_logs::{
        DeploymentLogOwner, DeploymentLogOwnerFuture, NodeDeploymentLogs, NodeLogRequest,
        NodeLogSelection, OwnerLogSources, PreparationLogs, PreparationOwner, SourceSelection,
        WorkloadLifecycle, model_steps, step_source,
    },
    dispatch::model::{RuntimeTargetError, RuntimeTargetMap},
    management_logs::{
        DeploymentLogError, DeploymentLogFollowQuery, DeploymentLogReadQuery, DeploymentLogStreams,
        ProtectedLogValues,
    },
};
use crate::{model::domain::ModelDeploymentId, model::state::CurrentStateRepository};

pub(crate) const MODEL_LOG_SOURCE_NAME: &str = "model";
pub(crate) const MODEL_LOG_SOURCE_LABEL: &str = "Model";

/// Read-only owner boundary for one model deployment's runtime diagnostics.
pub trait ModelDeploymentLogController: Send + Sync {
    fn read_deployment_logs(
        &self,
        deployment_id: ModelDeploymentId,
        query: DeploymentLogReadQuery,
    ) -> impl Future<Output = Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>> + Send;

    fn follow_deployment_logs(
        &self,
        deployment_id: ModelDeploymentId,
        query: DeploymentLogFollowQuery,
    ) -> impl Future<Output = Result<DeploymentLogStreams, DeploymentLogError>> + Send;

    fn protected_log_values(
        &self,
        deployment_id: ModelDeploymentId,
    ) -> impl Future<Output = Result<ProtectedLogValues, DeploymentLogError>> + Send;
}

impl<D, S> ModelDeploymentLogController for InferenceController<D, S>
where
    D: DesiredDeploymentRepository + 'static,
    S: CurrentStateRepository + 'static,
{
    fn read_deployment_logs(
        &self,
        deployment_id: ModelDeploymentId,
        query: DeploymentLogReadQuery,
    ) -> impl Future<Output = Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>> + Send {
        let shared = self.inner.clone();
        async move {
            let location = locate(&shared, &deployment_id, query.source_names())?;
            location
                .logs
                .read(location.request(&deployment_id), &query)
                .await
        }
    }

    fn follow_deployment_logs(
        &self,
        deployment_id: ModelDeploymentId,
        query: DeploymentLogFollowQuery,
    ) -> impl Future<Output = Result<DeploymentLogStreams, DeploymentLogError>> + Send {
        let shared = self.inner.clone();
        async move {
            let location = locate(&shared, &deployment_id, query.source_names())?;
            location
                .logs
                .follow(location.request(&deployment_id), &query)
                .await
        }
    }

    fn protected_log_values(
        &self,
        deployment_id: ModelDeploymentId,
    ) -> impl Future<Output = Result<ProtectedLogValues, DeploymentLogError>> + Send {
        let shared = self.inner.clone();
        async move {
            let snapshot = shared
                .desired
                .declaration_snapshot()
                .map_err(DeploymentLogError::configuration)?;
            let deployment = snapshot
                .deployments
                .iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id.0)
                .ok_or(DeploymentLogError::NotFound)?;
            let values = deployment
                .upstream_desired_configuration
                .as_ref()
                .into_iter()
                .flat_map(|configuration| {
                    std::iter::once(configuration.api_key.clone()).chain(
                        configuration
                            .endpoint
                            .as_ref()
                            .map(|endpoint| endpoint.base_url.clone()),
                    )
                });
            Ok(ProtectedLogValues::new(values))
        }
    }
}

impl<D, S> InferenceController<D, S> {
    /// Installs the manager's reader of deployment logs on their hosting nodes.
    pub(crate) fn set_node_log_source(&self, logs: NodeDeploymentLogs) {
        *self
            .inner
            .node_logs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(logs);
    }
}

/// Node-side log owner for the model deployments assigned in the node's installed view.
///
/// It serves the preparation steps the node retains for a deployment ahead of its workload,
/// and resolves each workload from the realizer's own input, with the engine launch identities
/// this node pinned, the node's own runtime targets, and its artifact cache, exactly as the
/// node's realizer launches it. Until that realizer has started, while no view is installed,
/// or while a deployment waits for its engine pin, no workload can exist, so its workload source
/// reads as not created yet while its steps stay readable. A follow of every source also
/// receives the workload changes the realizer publishes, so it can announce the workload again.
pub(crate) struct NodeModelLogs<D> {
    input: D,
    runtimes: RuntimeTargetMap,
    artifacts: ArtifactManager,
    realizer_started: Arc<AtomicBool>,
    preparation: PreparationLogs,
    lifecycle: WorkloadLifecycle,
}

impl<D: ModelRealizationInput> NodeModelLogs<D> {
    pub(crate) fn new(
        input: D,
        runtimes: RuntimeTargetMap,
        artifacts: ArtifactManager,
        realizer_started: Arc<AtomicBool>,
        preparation: PreparationLogs,
        lifecycle: WorkloadLifecycle,
    ) -> Self {
        Self {
            input,
            runtimes,
            artifacts,
            realizer_started,
            preparation,
            lifecycle,
        }
    }

    fn resolve(
        &self,
        deployment_id: &str,
        requested_sources: &[String],
    ) -> Result<NodeLogSource, DeploymentLogError> {
        let resolved = resolve_runtime_source(
            &self.input,
            &self.runtimes,
            &self.artifacts,
            deployment_id,
            requested_sources,
        );
        match resolved {
            Ok(source) if self.realizer_started.load(Ordering::Acquire) => {
                Ok(NodeLogSource::Readable(source))
            }
            Ok(source) => Ok(NodeLogSource::NotCreated(source.source)),
            // Without an installed view, as after a manager restart cleared it, this incarnation
            // has realized nothing yet.
            Err(DeploymentLogError::Configuration(error)) if no_installed_view(&error) => {
                let selection = select_sources(requested_sources, &[model_source()])?;
                Ok(NodeLogSource::NotCreated(selection.sources()[0].clone()))
            }
            // An assigned deployment still waiting for its engine pin has no workload yet.
            Err(DeploymentLogError::NotFound) if self.is_withheld(deployment_id)? => {
                let selection = select_sources(requested_sources, &[model_source()])?;
                Ok(NodeLogSource::NotCreated(selection.sources()[0].clone()))
            }
            Err(error) => Err(error),
        }
    }

    /// Whether `deployment_id` is assigned but withheld from the realizer: its engine is not
    /// pinned on this node yet, or its launch does not resolve.
    fn is_withheld(&self, deployment_id: &str) -> Result<bool, DeploymentLogError> {
        Ok(self
            .input
            .withheld_deployments()
            .map_err(DeploymentLogError::configuration)?
            .iter()
            .any(|deployment| deployment.model_deployment_id == deployment_id))
    }

    /// Splits the requested sources into the deployment's steps and its workload source.
    fn select(&self, deployment_id: &str, requested_sources: &[String]) -> SourceSelection {
        self.preparation.select(
            PreparationOwner::Model,
            deployment_id,
            requested_sources,
            &model_steps(),
            1,
        )
    }
}

impl<D: ModelRealizationInput> DeploymentLogOwner for NodeModelLogs<D> {
    fn read_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogReadQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceHistory>> {
        // Node model deployments are never upstream, so no launch value is protected, in step
        // lines either.
        Box::pin(async move {
            let selection = self.select(deployment_id, query.source_names());
            // Only a selected workload source is resolved, so steps stay readable whatever its
            // resolution would report.
            let workload = selection
                .workload
                .as_deref()
                .map(|names| self.resolve(deployment_id, names))
                .transpose()?;
            let mut histories = self
                .preparation
                .read(deployment_id, &selection.steps, query.history())
                .await?;
            if let Some(workload) = workload {
                histories.extend(match workload {
                    NodeLogSource::Readable(source) => {
                        read_local(&self.runtimes, source, query.history()).await?
                    }
                    NodeLogSource::NotCreated(source) => vec![RuntimeLogSourceHistory {
                        source,
                        result: Err(RuntimeBackendError::workload_not_created()),
                    }],
                });
            }
            Ok(OwnerLogSources::unprotected(histories))
        })
    }

    fn follow_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogFollowQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceStream>> {
        Box::pin(async move {
            // Subscribing before resolving the workload means no change falls between the two.
            let changes = query
                .source_names()
                .is_empty()
                .then(|| self.lifecycle.subscribe(deployment_id));
            let selection = self.select(deployment_id, query.source_names());
            let workload = selection
                .workload
                .as_deref()
                .map(|names| self.resolve(deployment_id, names))
                .transpose()?;
            let mut streams = self.preparation.follow(
                deployment_id,
                &selection.steps,
                query.resume_positions(),
                query.cancellation(),
            );
            // A follow of every source also lists the steps that start while it runs.
            let late = query.source_names().is_empty().then(|| {
                self.preparation.follow_started(
                    PreparationOwner::Model,
                    deployment_id,
                    model_steps().to_vec(),
                    &selection.steps,
                    1,
                    &query,
                )
            });
            if let Some(workload) = workload {
                streams.extend(match workload {
                    NodeLogSource::Readable(source) => {
                        follow_local(&self.runtimes, source, &query).await?
                    }
                    NodeLogSource::NotCreated(source) => vec![RuntimeLogSourceStream {
                        source,
                        result: Err(RuntimeBackendError::workload_not_created()),
                    }],
                });
            }
            Ok(OwnerLogSources::unprotected(streams)
                .with_late(late)
                .with_changes(changes))
        })
    }
}

/// How a node reads one deployment's logs.
enum NodeLogSource {
    /// Its workload may exist, so the exact local runtime is read.
    Readable(LocalSource),

    /// No workload can exist yet, so the source reads as not created.
    NotCreated(RuntimeLogSource),
}

/// The hosting node a deployment's logs are read from, with the owner's selected sources.
struct LogLocation {
    logs: NodeDeploymentLogs,
    node_id: String,

    /// Every source the deployment may have.
    known: Vec<RuntimeLogSource>,

    /// The requested sources, or the workload source when the request names none.
    sources: Vec<RuntimeLogSource>,

    /// Whether the request names no source, so the node answers with every source it holds.
    all: bool,
}

impl LogLocation {
    fn request<'a>(&'a self, deployment_id: &'a ModelDeploymentId) -> NodeLogRequest<'a> {
        NodeLogRequest {
            node_id: &self.node_id,
            owner_kind: DeploymentOwnerKind::Model,
            deployment_id: &deployment_id.0,
            selection: if self.all {
                NodeLogSelection::All {
                    known: &self.known,
                    defaults: &self.sources,
                }
            } else {
                NodeLogSelection::Named(&self.sources)
            },
        }
    }
}

/// The exact local workload and public source that one deployment's logs are read from.
struct LocalSource {
    execution_target_id: String,
    correlation: WorkloadCorrelation,
    source: RuntimeLogSource,
}

/// Resolves the hosting node of a declared node-hosted deployment. Upstream deployments have no
/// runtime logs.
fn locate<D, S>(
    shared: &super::ManagerInferenceControllerInner<D, S>,
    deployment_id: &ModelDeploymentId,
    requested_sources: &[String],
) -> Result<LogLocation, DeploymentLogError>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let declared = shared
        .desired
        .model_deployments()
        .map_err(DeploymentLogError::configuration)?
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id.0)
        .ok_or(DeploymentLogError::NotFound)?;
    if super::is_upstream_deployment(&declared) {
        return Err(DeploymentLogError::UnsupportedDeployment);
    }
    let known = model_steps()
        .iter()
        .map(step_source)
        .chain([model_source()])
        .collect::<Vec<_>>();
    let all = requested_sources.is_empty();
    let sources = if all {
        vec![model_source()]
    } else {
        select_sources(requested_sources, &known)?
            .sources()
            .to_vec()
    };
    let Some(logs) = shared
        .node_logs
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    else {
        return Err(DeploymentLogError::NotFound);
    };
    let node_id = declared
        .placement
        .map(|placement| placement.site_node_id)
        .ok_or(DeploymentLogError::NotFound)?;
    Ok(LogLocation {
        logs,
        node_id,
        known,
        sources,
        all,
    })
}

fn resolve_runtime_source(
    desired: &impl ModelRealizationInput,
    runtimes: &RuntimeTargetMap,
    artifacts: &ArtifactManager,
    deployment_id: &str,
    requested_sources: &[String],
) -> Result<LocalSource, DeploymentLogError> {
    let site = super::construction::site_with_deployments(desired, runtimes)
        .map_err(DeploymentLogError::configuration)?;
    let deployment = site
        .model_deployments
        .iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .ok_or(DeploymentLogError::NotFound)?;
    let source = model_source();
    select_sources(requested_sources, std::slice::from_ref(&source))?;
    let target = selected_target(&site, deployment).map_err(DeploymentLogError::configuration)?;
    let resolved = match resolved_launch(deployment) {
        Ok(resolved) => resolved,
        // A teardown whose engine was never pinned has no launch, and so no logs to read.
        Err(_) if is_absent(deployment) => return Err(DeploymentLogError::NotFound),
        Err(error) => return Err(DeploymentLogError::configuration(error)),
    };
    let artifacts = artifacts
        .expected_model_paths(&resolved.artifacts)
        .map_err(DeploymentLogError::configuration)?;
    let launch = workload_launch(deployment, target, &artifacts)
        .map_err(DeploymentLogError::configuration)?;
    Ok(LocalSource {
        execution_target_id: target.execution_target_id.clone(),
        correlation: launch.correlation,
        source,
    })
}

async fn read_local(
    runtimes: &RuntimeTargetMap,
    local: LocalSource,
    history: &RuntimeLogQuery,
) -> Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError> {
    let query = RuntimeWorkloadLogQuery::new(local.source.clone(), history.clone());
    let result = runtimes
        .read_workload_logs_by_correlation(&local.execution_target_id, &local.correlation, &query)
        .await;
    Ok(vec![RuntimeLogSourceHistory {
        source: local.source,
        result: per_source_result(result)?,
    }])
}

async fn follow_local(
    runtimes: &RuntimeTargetMap,
    local: LocalSource,
    query: &DeploymentLogFollowQuery,
) -> Result<Vec<RuntimeLogSourceStream>, DeploymentLogError> {
    let runtime_query = RuntimeWorkloadLogFollowQuery::new(
        local.source.clone(),
        query.source_query(&local.source.name),
    );
    let result = runtimes
        .follow_workload_logs_by_correlation(
            &local.execution_target_id,
            &local.correlation,
            runtime_query,
        )
        .await;
    Ok(vec![RuntimeLogSourceStream {
        source: local.source,
        result: per_source_result(result)?,
    }])
}

/// Whether a resolution failed only because the node has no installed view.
fn no_installed_view(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        matches!(
            source.downcast_ref::<SiteRepositoryError>(),
            Some(SiteRepositoryError::NoAcceptedView)
        )
    })
}

fn model_source() -> RuntimeLogSource {
    RuntimeLogSource {
        name: MODEL_LOG_SOURCE_NAME.to_owned(),
        label: MODEL_LOG_SOURCE_LABEL.to_owned(),
    }
}

fn select_sources(
    requested: &[String],
    declared: &[RuntimeLogSource],
) -> Result<RuntimeLogSourceSelection, DeploymentLogError> {
    let selected = if requested.is_empty() {
        declared.to_vec()
    } else {
        requested
            .iter()
            .map(|name| {
                declared
                    .iter()
                    .find(|source| source.name == *name)
                    .cloned()
                    .ok_or(DeploymentLogError::InvalidSource)
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    RuntimeLogSourceSelection::new(selected).map_err(DeploymentLogError::configuration)
}

fn per_source_result<T>(
    result: Result<T, RuntimeTargetError>,
) -> Result<Result<T, dotcore_runtime::RuntimeBackendError>, DeploymentLogError> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(RuntimeTargetError::Adapter { source, .. }) => Ok(Err(source)),
        Err(error) => Err(DeploymentLogError::configuration(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::ready,
        num::NonZeroU16,
        sync::{Arc, Mutex},
    };

    use dotcore_artifact::PreparationStep;
    use dotcore_domain::{clock::SystemClock, timing::Timing};
    use dotcore_proto::dotcore::deployment::private::v1::{
        DeploymentLogFollowOpened, DeploymentLogSource, DeploymentLogSourceOpening,
        DeploymentLogTarget, DeploymentLogUnavailableReason, FollowDeploymentLogsRequest,
        FollowDeploymentLogsResponse, deployment_log_service_server::DeploymentLogService as _,
        follow_deployment_logs_response::Frame,
    };
    use dotcore_runtime::{
        RuntimeBackend, RuntimeBackendId, RuntimeFuture, RuntimeLogEntry, RuntimeLogHistory,
        RuntimeLogStreamKind, WorkloadId, WorkloadLaunch, WorkloadObservation,
    };
    use dotcore_site::{
        private_transport::{AuthenticatedMachineIdentity, PrivateNodeRole, PrivateServiceContext},
        view::fencing::{
            AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
        },
    };
    use futures_util::{StreamExt as _, stream};

    use super::*;
    use crate::{
        deployment_logs::{DeploymentLogOwners, DeploymentLogReader, DeploymentLogService},
        dispatch::model::RuntimeTargetMapBuilder,
        model::controller::test_support::{
            TestCurrent, TestDesired, ready_oci_preparer, test_desired_deployment,
            test_simulated_runtime_target, test_site, valid_upstream_desired_deployment,
        },
    };

    #[tokio::test]
    async fn node_model_logs_use_exact_declared_correlation_without_lifecycle_or_discovery() {
        let directory = tempfile::tempdir().expect("preparation log directory");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("fixture-model");
        let current = TestCurrent::new(calls.clone());
        let runtime_calls = Arc::new(Mutex::new(Vec::new()));
        let mut runtimes = RuntimeTargetMapBuilder::new();
        runtimes
            .insert(
                "target-one",
                LogRuntime {
                    calls: runtime_calls.clone(),
                },
            )
            .expect("log runtime");
        let logs = NodeModelLogs::new(
            desired.clone(),
            runtimes.build().expect("Runtime map"),
            ArtifactManager::new("/test-cache".into(), Some(ready_oci_preparer()))
                .expect("artifact paths"),
            Arc::new(AtomicBool::new(true)),
            PreparationLogs::on_system_clock(directory.path()),
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        drop(current);
        calls.lock().expect("calls").clear();
        let query = DeploymentLogReadQuery::new(
            Vec::new(),
            dotcore_runtime::RuntimeLogQuery::new(NonZeroU16::new(2).expect("limit"), None)
                .expect("query"),
        );

        let history = logs
            .read_logs("deployment", query)
            .await
            .expect("history")
            .sources;

        assert!(calls.lock().expect("calls").is_empty());
        let runtime_calls = runtime_calls.lock().expect("Runtime calls");
        assert_eq!(runtime_calls.len(), 1);
        assert_eq!(runtime_calls[0].0, "read");
        assert_eq!(runtime_calls[0].1.owner_id, "deployment");
        assert_ne!(runtime_calls[0].1.reconcile_generation, 0);
        assert_eq!(history[0].source.name, "model");
        assert_eq!(history[0].source.label, "Model");
        let source_history = history[0].result.as_ref().expect("source history");
        assert_eq!(source_history.entries[0].content, b"owned");
        assert!(source_history.has_older_output);
        assert!(!source_history.candidate_budget_truncated);
    }

    #[tokio::test]
    async fn node_model_logs_serve_installed_assignments_once_the_realizer_started() {
        let directory = tempfile::tempdir().expect("preparation log directory");
        let view = AcceptedNodeViewAuthority::new(
            "site-one",
            "node-one",
            NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("session"),
            std::time::Duration::from_secs(30),
        )
        .expect("view authority");
        let install = |revision, deployments| {
            let projection = NodeViewProjection::new(test_site().execution_targets, deployments)
                .expect("node view projection");
            view.install_for_test("site-one", "node-one", revision, &projection)
                .expect("installed view");
        };
        install(
            1,
            vec![test_desired_deployment(
                "fixture-model",
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
            )],
        );
        let runtimes = test_simulated_runtime_target();
        let artifacts = ArtifactManager::new("/test-cache".into(), None).expect("artifact paths");
        let realizer_started = Arc::new(AtomicBool::new(false));
        let logs = NodeModelLogs::new(
            super::super::NodeViewModelInput::new(view.model_input()),
            runtimes.clone(),
            artifacts.clone(),
            Arc::clone(&realizer_started),
            PreparationLogs::on_system_clock(directory.path()),
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        let local = resolve_runtime_source(
            &super::super::NodeViewModelInput::new(view.model_input()),
            &runtimes,
            &artifacts,
            "deployment",
            &[],
        )
        .expect("installed deployment resolves");
        let site = super::super::construction::site_with_deployments(
            &super::super::NodeViewModelInput::new(view.model_input()),
            &runtimes,
        )
        .expect("Site projection");
        let deployment = &site.model_deployments[0];
        let launch = workload_launch(
            deployment,
            selected_target(&site, deployment).expect("target"),
            &artifacts
                .expected_model_paths(&resolved_launch(deployment).expect("launch").artifacts)
                .expect("artifact paths"),
        )
        .expect("workload launch");
        assert_eq!(launch.correlation, local.correlation);
        runtimes
            .create_workload("target-one", &launch)
            .await
            .expect("create simulated workload");
        let query = |sources: Vec<String>| {
            DeploymentLogReadQuery::new(
                sources,
                RuntimeLogQuery::new(NonZeroU16::new(2).expect("limit"), None).expect("query"),
            )
        };

        let before_realizer = logs
            .read_logs("deployment", query(Vec::new()))
            .await
            .expect("history before the realizer")
            .sources;
        realizer_started.store(true, Ordering::Release);
        let paused = logs
            .read_logs("deployment", query(Vec::new()))
            .await
            .expect("history of a Paused assignment")
            .sources;
        let invalid = logs
            .read_logs("deployment", query(vec!["engine".to_owned()]))
            .await;
        install(2, Vec::new());
        let withdrawn = logs.read_logs("deployment", query(Vec::new())).await;

        assert_eq!(before_realizer[0].source, model_source());
        assert!(matches!(
            before_realizer[0].result,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
        assert_eq!(
            paused[0]
                .result
                .as_ref()
                .expect("Paused history")
                .entries
                .iter()
                .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
                .collect::<Vec<_>>(),
            [11, 12]
                .map(|line| format!(
                    "simulated workload log line {line:03} owner=deployment generation={}",
                    launch.correlation.reconcile_generation
                ))
                .to_vec()
        );
        assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
        assert!(matches!(withdrawn, Err(DeploymentLogError::NotFound)));
    }

    #[tokio::test]
    async fn node_model_logs_serve_preparation_steps_ahead_of_a_workload_not_created_yet() {
        let directory = tempfile::tempdir().expect("preparation log directory");
        let view = AcceptedNodeViewAuthority::new(
            "site-one",
            "node-one",
            NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("session"),
            std::time::Duration::from_secs(30),
        )
        .expect("view authority");
        let projection = NodeViewProjection::new(
            test_site().execution_targets,
            vec![test_desired_deployment(
                "fixture-model",
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
            )],
        )
        .expect("node view projection");
        view.install_for_test("site-one", "node-one", 1, &projection)
            .expect("installed view");
        let preparation = PreparationLogs::on_system_clock(directory.path());
        let runtimes = test_simulated_runtime_target();
        let artifacts = ArtifactManager::new("/test-cache".into(), None).expect("artifact paths");
        // The realizer has not started, so the deployment is mid-preparation.
        let realizer_started = Arc::new(AtomicBool::new(false));
        let lifecycle = WorkloadLifecycle::new();
        let logs = Arc::new(NodeModelLogs::new(
            super::super::NodeViewModelInput::new(view.model_input()),
            runtimes.clone(),
            artifacts.clone(),
            Arc::clone(&realizer_started),
            preparation.clone(),
            lifecycle.clone(),
        ));
        let time = |second| std::time::UNIX_EPOCH + std::time::Duration::from_secs(second);
        let build = preparation.writer("deployment", PreparationStep::EngineBuild);
        build.append(time(20), RuntimeLogStreamKind::Stderr, b"building engine");
        build.append(time(21), RuntimeLogStreamKind::Stderr, b"copying path");
        // Both steps are still writing, so their follows stay open.
        let download = preparation.writer("deployment", PreparationStep::ModelDownload);
        download.append(
            time(10),
            RuntimeLogStreamKind::Stdout,
            b"downloading weights",
        );
        let query = |sources: &[&str]| {
            DeploymentLogReadQuery::new(
                sources.iter().map(|source| (*source).to_owned()).collect(),
                RuntimeLogQuery::new(NonZeroU16::new(10).expect("limit"), None).expect("query"),
            )
        };

        let all = logs
            .read_logs("deployment", query(&[]))
            .await
            .expect("history mid-preparation")
            .sources;
        let filtered = logs
            .read_logs("deployment", query(&["prepare:engine-build"]))
            .await
            .expect("one step's history")
            .sources;
        let not_started = logs
            .read_logs("deployment", query(&["prepare:engine-image"]))
            .await
            .expect("a step that has not started")
            .sources;
        let invalid = logs
            .read_logs("deployment", query(&["prepare:web:build"]))
            .await;
        let mut follow = logs
            .follow_logs(
                "deployment",
                DeploymentLogFollowQuery::new(
                    vec!["prepare:engine-build".to_owned()],
                    std::collections::BTreeMap::from([(
                        "prepare:engine-build".to_owned(),
                        time(20),
                    )]),
                    tokio_util::sync::CancellationToken::new(),
                ),
            )
            .await
            .expect("follow of one step")
            .sources;
        let mut follow = follow.remove(0).result.expect("an open step follow");
        let resumed = follow.next().await;
        build.append(time(22), RuntimeLogStreamKind::Stdout, b"engine built");
        let appended = follow.next().await;
        let mut every_source = node_follow_of_every_source(&view, &logs).await;
        let opened = next_follow_frame(&mut every_source).await;
        let mut step_entries = Vec::new();
        for _ in 0..4 {
            step_entries.push(follow_entry_text(
                next_follow_frame(&mut every_source).await,
            ));
        }
        // The realizer starts, creates the engine workload and publishes it.
        let launch = installed_launch(&view, &runtimes, &artifacts);
        runtimes
            .create_workload("target-one", &launch)
            .await
            .expect("create simulated workload");
        realizer_started.store(true, Ordering::Release);
        lifecycle.started("deployment", MODEL_LOG_SOURCE_NAME);
        let announced = next_follow_frame(&mut every_source).await;
        let first_line = follow_entry_text(next_follow_frame(&mut every_source).await);

        assert_eq!(
            all.iter()
                .map(|history| (
                    history.source.name.as_str(),
                    history.source.label.as_str(),
                    history
                        .result
                        .as_ref()
                        .ok()
                        .map(|history| entry_texts(&history.entries)),
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "prepare:download",
                    "Model download",
                    Some(vec!["downloading weights".to_owned()])
                ),
                (
                    "prepare:engine-build",
                    "Engine build",
                    Some(vec![
                        "building engine".to_owned(),
                        "copying path".to_owned()
                    ])
                ),
                ("model", "Model", None),
            ]
        );
        assert!(matches!(
            all[2].result,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
        assert_eq!(
            filtered
                .iter()
                .map(|history| history.source.name.as_str())
                .collect::<Vec<_>>(),
            ["prepare:engine-build"]
        );
        assert_eq!(
            entry_texts(
                &not_started[0]
                    .result
                    .as_ref()
                    .expect("an available step")
                    .entries
            ),
            Vec::<String>::new()
        );
        assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
        assert!(matches!(
            resumed,
            Some(Ok(dotcore_runtime::RuntimeLogEvent::Entry(entry))) if entry.content == b"copying path"
        ));
        assert!(matches!(
            appended,
            Some(Ok(dotcore_runtime::RuntimeLogEvent::Entry(entry)))
                if entry.content == b"engine built" && entry.stream == RuntimeLogStreamKind::Stdout
        ));
        assert_eq!(
            opened,
            Frame::Opened(DeploymentLogFollowOpened {
                sources: vec![
                    follow_opening("prepare:download", "Model download", None),
                    follow_opening("prepare:engine-build", "Engine build", None),
                    follow_opening(
                        "model",
                        "Model",
                        Some(DeploymentLogUnavailableReason::WorkloadNotCreated),
                    ),
                ],
            })
        );
        step_entries.sort();
        assert_eq!(
            step_entries,
            [
                (
                    "prepare:download".to_owned(),
                    "downloading weights".to_owned()
                ),
                (
                    "prepare:engine-build".to_owned(),
                    "building engine".to_owned()
                ),
                ("prepare:engine-build".to_owned(), "copying path".to_owned()),
                ("prepare:engine-build".to_owned(), "engine built".to_owned()),
            ]
        );
        assert_eq!(
            announced,
            Frame::SourceOpened(follow_opening("model", "Model", None)),
            "the follow opened during preparation announces the created workload"
        );
        assert_eq!(
            first_line,
            (
                "model".to_owned(),
                format!(
                    "simulated workload log line 001 owner=deployment generation={}",
                    launch.correlation.reconcile_generation
                )
            ),
            "the announced workload replays from its first line"
        );
    }

    #[tokio::test]
    async fn node_model_logs_serve_named_steps_whatever_the_workload_resolution() {
        let directory = tempfile::tempdir().expect("preparation log directory");
        let view = AcceptedNodeViewAuthority::new(
            "site-one",
            "node-one",
            NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("session"),
            std::time::Duration::from_secs(30),
        )
        .expect("view authority");
        // The installed view no longer assigns the deployment, as while its teardown finishes.
        let projection = NodeViewProjection::new(test_site().execution_targets, Vec::new())
            .expect("node view projection");
        view.install_for_test("site-one", "node-one", 1, &projection)
            .expect("installed view");
        let preparation = PreparationLogs::on_system_clock(directory.path());
        preparation
            .writer("deployment", PreparationStep::EngineBuild)
            .append(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1),
                RuntimeLogStreamKind::Stderr,
                b"building engine",
            );
        let logs = NodeModelLogs::new(
            super::super::NodeViewModelInput::new(view.model_input()),
            test_simulated_runtime_target(),
            ArtifactManager::new("/test-cache".into(), None).expect("artifact paths"),
            Arc::new(AtomicBool::new(true)),
            preparation,
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        let query = |sources: &[&str]| {
            DeploymentLogReadQuery::new(
                sources.iter().map(|source| (*source).to_owned()).collect(),
                RuntimeLogQuery::new(NonZeroU16::new(10).expect("limit"), None).expect("query"),
            )
        };

        let step = logs
            .read_logs("deployment", query(&["prepare:engine-build"]))
            .await
            .expect("a named step is read without resolving the workload")
            .sources;
        let with_workload = logs
            .read_logs("deployment", query(&["prepare:engine-build", "model"]))
            .await;

        assert_eq!(
            entry_texts(&step[0].result.as_ref().expect("step history").entries),
            ["building engine"]
        );
        assert!(matches!(with_workload, Err(DeploymentLogError::NotFound)));
    }

    #[tokio::test]
    async fn node_model_logs_without_an_installed_view_read_as_not_created() {
        let directory = tempfile::tempdir().expect("preparation log directory");
        let view = AcceptedNodeViewAuthority::new(
            "site-one",
            "node-one",
            NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("session"),
            std::time::Duration::from_secs(30),
        )
        .expect("view authority");
        let logs = NodeModelLogs::new(
            super::super::NodeViewModelInput::new(view.model_input()),
            test_simulated_runtime_target(),
            ArtifactManager::new("/test-cache".into(), None).expect("artifact paths"),
            Arc::new(AtomicBool::new(true)),
            PreparationLogs::on_system_clock(directory.path()),
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        let query = |sources: Vec<String>| {
            DeploymentLogReadQuery::new(
                sources,
                RuntimeLogQuery::new(NonZeroU16::new(2).expect("limit"), None).expect("query"),
            )
        };

        let history = logs
            .read_logs("deployment", query(Vec::new()))
            .await
            .expect("no installed view is an expected outcome")
            .sources;
        let follow = logs
            .follow_logs(
                "deployment",
                DeploymentLogFollowQuery::new(
                    Vec::new(),
                    Default::default(),
                    tokio_util::sync::CancellationToken::new(),
                ),
            )
            .await
            .expect("no installed view is an expected outcome")
            .sources;
        let invalid = logs
            .read_logs("deployment", query(vec!["engine".to_owned()]))
            .await;

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].source, model_source());
        assert!(matches!(
            history[0].result,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
        assert_eq!(follow.len(), 1);
        assert!(matches!(
            follow[0].result,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
        assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
    }

    #[tokio::test]
    async fn protected_values_are_fresh_owner_values_and_never_projected_as_identity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let mut upstream = valid_upstream_desired_deployment();
        upstream.model_deployment_id = "private-upstream".to_owned();
        let configuration = upstream
            .upstream_desired_configuration
            .as_mut()
            .expect("configuration");
        configuration.api_key = "first-secret".to_owned();
        configuration.endpoint.as_mut().expect("endpoint").base_url =
            "https://private.example/v1".to_owned();
        desired
            .state
            .lock()
            .expect("desired")
            .model_deployments
            .push(upstream);
        let controller = InferenceController::new(
            desired.clone(),
            TestCurrent::new(calls),
            crate::model::controller::test_support::test_target_capability_source(),
            Timing::test().model,
            Arc::new(SystemClock),
        )
        .expect("model owner");
        let id = ModelDeploymentId("private-upstream".to_owned());

        let first = controller
            .protected_log_values(id.clone())
            .await
            .expect("protected values");
        desired.state.lock().expect("desired").model_deployments[0]
            .upstream_desired_configuration
            .as_mut()
            .expect("configuration")
            .api_key = "replacement-secret".to_owned();
        let replacement = controller
            .protected_log_values(id)
            .await
            .expect("fresh protected values");

        // Protected values are kept longest first.
        assert_eq!(
            first.values(),
            [
                b"https://private.example/v1".to_vec(),
                b"first-secret".to_vec()
            ]
        );
        assert_eq!(
            replacement.values(),
            [
                b"https://private.example/v1".to_vec(),
                b"replacement-secret".to_vec()
            ]
        );
        assert!(
            replacement
                .values()
                .iter()
                .all(|value| value != b"private-upstream")
        );
    }

    fn entry_texts(entries: &[RuntimeLogEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
            .collect()
    }

    /// A follow of every source of `deployment` through the node's deployment-log service, as
    /// the manager opens it.
    async fn node_follow_of_every_source(
        view: &AcceptedNodeViewAuthority,
        logs: &Arc<NodeModelLogs<super::super::NodeViewModelInput>>,
    ) -> impl futures_util::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>> + Unpin
    {
        let service = DeploymentLogService::new(
            DeploymentLogReader::new(
                view.clone(),
                DeploymentLogOwners {
                    model: Arc::clone(logs) as Arc<dyn DeploymentLogOwner>,
                    application: None,
                },
                crate::management_logs::ManagementLogAdmission::new(),
            ),
            &PrivateServiceContext::new(PrivateNodeRole::Worker, "node-one"),
        );
        let mut request = tonic::Request::new(FollowDeploymentLogsRequest {
            target: Some(DeploymentLogTarget {
                manager_session_id: "a".repeat(32),
                node_incarnation_id: "1".repeat(32),
                owner_kind: DeploymentOwnerKind::Model as i32,
                deployment_id: "deployment".to_owned(),
            }),
            source_names: Vec::new(),
            resume_positions: Vec::new(),
        });
        request
            .extensions_mut()
            .insert(AuthenticatedMachineIdentity {
                site_id: "site-one".to_owned(),
                node_id: "manager-one".to_owned(),
                role: PrivateNodeRole::Manager,
            });
        service
            .follow_deployment_logs(request)
            .await
            .expect("an admitted follow")
            .into_inner()
    }

    /// The next frame of a node follow, which must arrive promptly.
    async fn next_follow_frame(
        follow: &mut (
                 impl futures_util::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>>
                 + Unpin
             ),
    ) -> Frame {
        tokio::time::timeout(std::time::Duration::from_secs(5), follow.next())
            .await
            .expect("a frame arrives promptly")
            .expect("an open follow")
            .expect("a follow frame")
            .frame
            .expect("a frame payload")
    }

    /// The source name and text of an entry frame.
    fn follow_entry_text(frame: Frame) -> (String, String) {
        match frame {
            Frame::Entry(entry) => (
                entry.source_name,
                String::from_utf8_lossy(&entry.entry.expect("an entry").content).into_owned(),
            ),
            frame => panic!("expected an entry frame, got {frame:?}"),
        }
    }

    fn follow_opening(
        name: &str,
        label: &str,
        unavailable: Option<DeploymentLogUnavailableReason>,
    ) -> DeploymentLogSourceOpening {
        DeploymentLogSourceOpening {
            source: Some(DeploymentLogSource {
                name: name.to_owned(),
                label: label.to_owned(),
            }),
            unavailable: unavailable.map(|reason| reason as i32),
        }
    }

    /// The launch the realizer creates for the one deployment of the installed view.
    fn installed_launch(
        view: &AcceptedNodeViewAuthority,
        runtimes: &RuntimeTargetMap,
        artifacts: &ArtifactManager,
    ) -> WorkloadLaunch {
        let site = super::super::construction::site_with_deployments(
            &super::super::NodeViewModelInput::new(view.model_input()),
            runtimes,
        )
        .expect("Site projection");
        let deployment = &site.model_deployments[0];
        workload_launch(
            deployment,
            selected_target(&site, deployment).expect("target"),
            &artifacts
                .expected_model_paths(&resolved_launch(deployment).expect("launch").artifacts)
                .expect("artifact paths"),
        )
        .expect("workload launch")
    }

    struct LogRuntime {
        calls: Arc<Mutex<Vec<(&'static str, dotcore_runtime::WorkloadCorrelation)>>>,
    }

    impl RuntimeBackend for LogRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            RuntimeBackendId("runtime/test".to_owned())
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            self.calls.lock().expect("calls").push((
                "discover",
                dotcore_runtime::WorkloadCorrelation {
                    owner_id: String::new(),
                    reconcile_generation: 0,
                },
            ));
            Box::pin(ready(Ok(Vec::new())))
        }

        fn create_workload<'a>(
            &'a self,
            _: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(
                dotcore_runtime::RuntimeBackendError::invalid_launch("unexpected lifecycle call"),
            )))
        }

        fn start_workload<'a>(
            &'a self,
            _: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(
                dotcore_runtime::RuntimeBackendError::invalid_launch("unexpected lifecycle call"),
            )))
        }

        fn inspect_workload<'a>(
            &'a self,
            _: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(
                dotcore_runtime::RuntimeBackendError::invalid_launch("unexpected lifecycle call"),
            )))
        }

        fn stop_workload<'a>(&'a self, _: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(ready(Err(
                dotcore_runtime::RuntimeBackendError::invalid_launch("unexpected lifecycle call"),
            )))
        }

        fn remove_workload<'a>(&'a self, _: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(ready(Err(
                dotcore_runtime::RuntimeBackendError::invalid_launch("unexpected lifecycle call"),
            )))
        }

        fn remove_storage<'a>(
            &'a self,
            _: &'a dotcore_runtime::WorkloadStorageKey,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn read_workload_logs_by_correlation<'a>(
            &'a self,
            correlation: &'a dotcore_runtime::WorkloadCorrelation,
            query: &'a RuntimeWorkloadLogQuery,
        ) -> dotcore_runtime::RuntimeLogFuture<'a, RuntimeLogHistory> {
            self.calls
                .lock()
                .expect("calls")
                .push(("read", correlation.clone()));
            Box::pin(ready(Ok(RuntimeLogHistory {
                entries: vec![RuntimeLogEntry {
                    source: query.source().clone(),
                    timestamp: std::time::UNIX_EPOCH,
                    stream: RuntimeLogStreamKind::Stdout,
                    content: b"owned".to_vec(),
                    truncated: false,
                }],
                has_older_output: true,
                candidate_budget_truncated: false,
            })))
        }

        fn follow_workload_logs_by_correlation<'a>(
            &'a self,
            correlation: &'a dotcore_runtime::WorkloadCorrelation,
            _: RuntimeWorkloadLogFollowQuery,
        ) -> dotcore_runtime::RuntimeLogFuture<'a, dotcore_runtime::RuntimeLogStream> {
            self.calls
                .lock()
                .expect("calls")
                .push(("follow", correlation.clone()));
            let stream: dotcore_runtime::RuntimeLogStream = Box::pin(stream::empty());
            Box::pin(ready(Ok(stream)))
        }
    }
}
