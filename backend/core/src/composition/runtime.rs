//! Runtime backend composition for configured site capabilities.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::{
    ExecutionPlatform, SiteNodeId, SitePaths,
    clock::Clock,
    timing::{NativeTiming, SimulatedTiming},
};
use dotcore_proto::dotcore::site::v1::{
    ExecutionTarget, application_runtime_binding, execution_target, model_runtime_binding,
};
use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeBackendError, RuntimeBackendId, RuntimeFuture,
    RuntimeGroupLogFollowQuery, RuntimeLogFuture, RuntimeLogQuery, RuntimeLogSourceHistory,
    RuntimeLogSourceSelection, RuntimeLogSourceStream, WorkloadGroupBackend, WorkloadGroupId,
    WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadMaterializationRequirements,
    WorkloadStorageKey, WorkloadUnitName, WorkloadUnitResourceObservation,
};
use dotcore_runtime_backends::{
    docker::{
        DockerAcceleratorTransport, DockerBackendFactory, DockerRuntimeFactory,
        validated_accelerator_transport,
    },
    native::{
        DEFAULT_STORE_ROOT, NativeLauncherError, NativeRuntimeBackend, NativeRuntimeConfig,
        NativeStateRoot, NativeWorkloadLifetime,
    },
    simulated::{
        DeclaredMachine, InMemoryRuntimeBackend, SimulatedRuntimeConfig,
        SimulatedRuntimeTestSupport, SimulatedStartupConfig,
    },
};
use dotcore_site::view::fencing::AcceptedNodeViewAuthority;
use tracing::{info, warn};

use super::{
    CompositionError,
    runtime_adapters::{
        ApplicationRuntimeAdapterFactory, ApplicationRuntimeAdapterFuture,
        ModelRuntimeAdapterFactory, ModelRuntimeAdapterFuture, RuntimeAdapterKey,
        RuntimeAdapterRegistry, RuntimeAdapterRegistryError,
    },
};
use crate::dispatch::{
    application::{ApplicationRuntimeMap, ApplicationRuntimeMapBuilder},
    model::{RuntimeTargetMap, RuntimeTargetMapBuilder},
};

const DOCKER_ADAPTER_KEY: RuntimeAdapterKey = RuntimeAdapterKey::new("docker");
const NATIVE_ADAPTER_KEY: RuntimeAdapterKey = RuntimeAdapterKey::new("native");
const SIMULATED_ADAPTER_KEY: RuntimeAdapterKey = RuntimeAdapterKey::new("simulated");

/// Builds the runtime adapter registry of SiteNode `node_id`. The native factory resolves its
/// launcher here, at `native_launcher` when given, but reports a missing launcher only when a
/// native execution target is composed. Simulated model targets run on `declared_machine` and
/// refuse to compose without one.
pub(super) fn runtime_adapter_registry_for_site_id(
    configuration: &SitePaths,
    site_id: &str,
    node_id: &SiteNodeId,
    native_launcher: Option<&Path>,
    native_timing: NativeTiming,
    simulated: SimulatedRuntimeInputs,
    docker_factory_override: Option<Arc<dyn DockerBackendFactory>>,
) -> Result<RuntimeAdapterRegistry, CompositionError> {
    let mut registry = runtime_adapter_registry_with_docker_factory(
        docker_factory_override.unwrap_or_else(|| Arc::new(docker_factory(configuration, site_id))),
        simulated,
    )?;
    registry
        .register_model(
            NATIVE_ADAPTER_KEY,
            Arc::new(NativeRuntimeAdapterFactory::new(
                configuration,
                site_id,
                node_id.clone(),
                native_launcher,
                native_timing,
            )),
        )
        .map_err(runtime_registration_error)?;
    Ok(registry)
}

pub(super) async fn node_runtime_targets_for(
    targets: &[ExecutionTarget],
    registry: &RuntimeAdapterRegistry,
    view: AcceptedNodeViewAuthority,
) -> Result<RuntimeTargetMap, CompositionError> {
    let mut builder = RuntimeTargetMapBuilder::leased(view);
    for target in targets.iter().filter(|target| {
        matches!(
            target.runtime_binding,
            Some(execution_target::RuntimeBinding::ModelRuntime(_))
        )
    }) {
        let execution_target::RuntimeBinding::ModelRuntime(binding) = target
            .runtime_binding
            .as_ref()
            .expect("filtered model execution target")
        else {
            unreachable!("filtered model execution target")
        };
        let adapter = binding.adapter.as_ref().ok_or_else(|| {
            CompositionError::composition(format!(
                "execution target {:?} requires one runtime adapter",
                target.execution_target_id
            ))
        })?;
        let key = model_runtime_adapter_key(adapter);
        let runtime = registry.model_backend(key, target).await?;
        builder.insert_boxed(&target.execution_target_id, runtime)?;
    }
    Ok(builder.build_allow_empty())
}

/// Builds the application runtime map of a node's own application targets, with every runtime
/// mutation fenced by the lease of the node's accepted `view`.
pub(super) async fn node_application_runtimes_for(
    targets: &[ExecutionTarget],
    registry: &RuntimeAdapterRegistry,
    view: AcceptedNodeViewAuthority,
    reconnect: DockerReconnect,
) -> Result<ApplicationRuntimeMap, CompositionError> {
    let mut builder = ApplicationRuntimeMapBuilder::leased(view);
    for target in targets {
        let Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) =
            target.runtime_binding.as_ref()
        else {
            continue;
        };
        let adapter = binding.adapter.as_ref().ok_or_else(|| {
            CompositionError::composition(
                "Application execution target requires one runtime adapter",
            )
        })?;
        let key = application_adapter_key(adapter);
        let runtime = match registry.application_backend(key, target).await {
            Ok(runtime) => {
                info!(event_kind = "runtime_selected", owner = "application", execution_target_id = %target.execution_target_id, runtime = key.as_str(), "selected node runtime adapter");
                runtime
            }
            // An unreachable Docker daemon leaves the target declared and unavailable instead of
            // failing the node: its declaration and its host and endpoint configuration stay, and
            // every other target composes and acts under the node's accepted view as before.
            Err(error) if is_unreachable_docker_application_target(key, &error) => {
                let Some(factory) = registry.application_factory(key) else {
                    return Err(error);
                };
                warn!(
                    event_kind = "runtime_target_unavailable",
                    owner = "application",
                    execution_target_id = %target.execution_target_id,
                    error = %dotcore_domain::diagnostic::diagnostic_chain(&error),
                    "Application target's Docker daemon could not be reached; the target stays declared and unavailable"
                );
                Box::new(DeferredDockerApplicationRuntime::new(
                    factory,
                    target.clone(),
                    reconnect.clone(),
                )) as Box<dyn dotcore_runtime::WorkloadGroupBackend>
            }
            Err(error) => return Err(error),
        };
        builder
            .insert_boxed(&target.execution_target_id, runtime)
            .map_err(|error| {
                CompositionError::composition_with_source("Application runtime map", error)
            })?;
    }
    Ok(builder.build())
}

/// Whether one application target's composition failure may leave it declared and unavailable
/// instead of failing the node: only a Docker application adapter whose own runtime reports a
/// backend failure. Invalid configuration and every other adapter still fail the node.
fn is_unreachable_docker_application_target(
    key: RuntimeAdapterKey,
    error: &CompositionError,
) -> bool {
    key == DOCKER_ADAPTER_KEY && matches!(error, CompositionError::RuntimeBackend(_))
}

/// When an unreachable Docker daemon is tried again: the backend clock and the retry window.
#[derive(Clone, Debug)]
pub(super) struct DockerReconnect {
    pub(super) clock: Arc<dyn Clock>,
    pub(super) interval: Duration,
}

/// The runtime of one Docker application target whose daemon could not be reached. It stands in
/// for the Docker backend so the target stays declared and unavailable: every operation opens the
/// connection it needs, an unreachable daemon reports the outage instead of a success, and the
/// target serves again as soon as the daemon answers, with no restart and no target change. After a
/// failed attempt, calls within the retry window fail with that outage without reconnecting.
struct DeferredDockerApplicationRuntime {
    factory: Arc<dyn ApplicationRuntimeAdapterFactory>,
    target: ExecutionTarget,
    connected: Mutex<Option<Arc<dyn WorkloadGroupBackend>>>,
    reconnect: DockerReconnect,

    /// When the last connection attempt failed and why.
    last_failure: Mutex<Option<ConnectFailure>>,
}

struct ConnectFailure {
    at: tokio::time::Instant,
    message: String,
}

impl DeferredDockerApplicationRuntime {
    fn new(
        factory: Arc<dyn ApplicationRuntimeAdapterFactory>,
        target: ExecutionTarget,
        reconnect: DockerReconnect,
    ) -> Self {
        Self {
            factory,
            target,
            connected: Mutex::new(None),
            reconnect,
            last_failure: Mutex::new(None),
        }
    }

    /// Returns the Docker backend of the target, opening the connection on the first attempt that
    /// reaches the daemon and keeping it for every later operation. Within the retry window after
    /// a failed attempt it reports that failure without connecting again.
    async fn backend(&self) -> Result<Arc<dyn WorkloadGroupBackend>, RuntimeBackendError> {
        if let Some(backend) = self
            .connected
            .lock()
            .expect("deferred runtime connection lock")
            .clone()
        {
            return Ok(backend);
        }
        if let Some(failure) = self
            .last_failure
            .lock()
            .expect("deferred runtime failure lock")
            .as_ref()
            .filter(|failure| {
                self.reconnect
                    .clock
                    .now()
                    .saturating_duration_since(failure.at)
                    < self.reconnect.interval
            })
        {
            return Err(RuntimeBackendError::Docker(anyhow::anyhow!(
                "{}",
                failure.message
            )));
        }
        let runtime = match self.factory.build(&self.target).await {
            Ok(runtime) => runtime,
            Err(error) => {
                let error = deferred_application_runtime_error(error);
                *self
                    .last_failure
                    .lock()
                    .expect("deferred runtime failure lock") = Some(ConnectFailure {
                    at: self.reconnect.clock.now(),
                    message: match &error {
                        RuntimeBackendError::Docker(source) => format!("{source:#}"),
                        other => other.to_string(),
                    },
                });
                return Err(error);
            }
        };
        *self
            .last_failure
            .lock()
            .expect("deferred runtime failure lock") = None;
        let backend = Arc::from(runtime);
        *self
            .connected
            .lock()
            .expect("deferred runtime connection lock") = Some(Arc::clone(&backend));
        Ok(backend)
    }
}

/// Keeps the daemon's own runtime failure when a reconnection fails for another reason, naming the
/// Docker target's reconnection as its context.
fn deferred_application_runtime_error(error: CompositionError) -> RuntimeBackendError {
    match error {
        CompositionError::RuntimeBackend(source) => source,
        other => RuntimeBackendError::docker_with_source(
            "Docker application runtime reconnect failed",
            other,
        ),
    }
}

impl WorkloadGroupBackend for DeferredDockerApplicationRuntime {
    // The target keeps reporting the Docker runtime it declares, and Docker always materializes
    // its own launch assets, so neither answer needs a connected daemon.
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(dotcore_runtime_backends::docker::BACKEND_ID.to_owned())
    }

    fn execution_platform<'a>(
        &'a self,
        execution_target_id: &'a str,
    ) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.execution_platform(execution_target_id).await
        })
    }

    fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
        WorkloadMaterializationRequirements::physical()
    }

    // Group discovery runs once for every application target of the node, so an unreachable daemon
    // observes no groups here instead of failing the whole pass and stopping the other targets from
    // reconciling. Their own operations still report the outage.
    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(async move {
            let backend = match self.backend().await {
                Ok(backend) => backend,
                Err(error) => {
                    warn!(
                        event_kind = "runtime_target_unavailable",
                        owner = "application",
                        execution_target_id = %self.target.execution_target_id,
                        error = %dotcore_domain::diagnostic::diagnostic_chain(&error),
                        "Application target's Docker daemon could not be reached during discovery; observing no workload groups"
                    );
                    return Ok(Vec::new());
                }
            };
            backend.discover_workload_groups().await
        })
    }

    fn storage_exists<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.storage_exists(storage).await
        })
    }

    fn remove_storage<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.remove_storage(storage).await
        })
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        group: &'a WorkloadGroupId,
        owner: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.ensure_workload_group_absent(group, owner).await
        })
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.create_workload_group(group).await
        })
    }

    fn start_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.start_workload_group(group).await
        })
    }

    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.inspect_workload_group(group).await
        })
    }

    fn observe_group_resources<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.observe_group_resources(group).await
        })
    }

    fn remove_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.remove_workload_group(group).await
        })
    }

    fn remove_observed_workload_group<'a>(
        &'a self,
        group: &'a ObservedWorkloadGroup,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.remove_observed_workload_group(group).await
        })
    }

    fn stop_workload_group<'a>(&'a self, group: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend.stop_workload_group(group).await
        })
    }

    fn read_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        selection: &'a RuntimeLogSourceSelection,
        query: &'a RuntimeLogQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceHistory>> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend
                .read_workload_group_logs(group, selection, query)
                .await
        })
    }

    fn follow_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        selection: &'a RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceStream>> {
        Box::pin(async move {
            let backend = self.backend().await?;
            backend
                .follow_workload_group_logs(group, selection, query)
                .await
        })
    }
}

/// What a node's simulated runtimes run with: their test controls, the machine the node
/// declares, and their timing on the backend clock.
pub(super) struct SimulatedRuntimeInputs {
    /// Call evidence, failure injection and lifecycle controls the simulated workloads follow;
    /// `None` runs them without controls.
    pub(super) test_support: Option<SimulatedRuntimeTestSupport>,

    /// Machine the node declares, on which its simulated model workloads and application units
    /// take memory.
    pub(super) machine: Option<DeclaredMachine>,

    /// Pace of the simulated workloads' representative startup.
    pub(super) timing: SimulatedTiming,

    /// Backend clock simulated workloads time their transitions and startup steps on.
    pub(super) clock: Arc<dyn Clock>,
}

impl SimulatedRuntimeInputs {
    /// Whether simulated workloads take representative time to start, as on a development site.
    fn representative_startup(&self) -> bool {
        self.test_support
            .as_ref()
            .is_some_and(SimulatedRuntimeTestSupport::representative_startup)
    }
}

pub(super) fn runtime_adapter_registry_with_docker_factory(
    docker_factory: Arc<dyn DockerBackendFactory>,
    simulated: SimulatedRuntimeInputs,
) -> Result<RuntimeAdapterRegistry, CompositionError> {
    let docker = Arc::new(DockerRuntimeAdapterFactory {
        factory: docker_factory,
    });
    let simulated = Arc::new(SimulatedRuntimeAdapterFactory { inputs: simulated });
    let mut registry = RuntimeAdapterRegistry::new();
    registry
        .register_model(DOCKER_ADAPTER_KEY, docker.clone())
        .map_err(runtime_registration_error)?;
    registry
        .register_application(DOCKER_ADAPTER_KEY, docker)
        .map_err(runtime_registration_error)?;
    registry
        .register_model(SIMULATED_ADAPTER_KEY, simulated.clone())
        .map_err(runtime_registration_error)?;
    registry
        .register_application(SIMULATED_ADAPTER_KEY, simulated)
        .map_err(runtime_registration_error)?;
    Ok(registry)
}

fn runtime_registration_error(error: RuntimeAdapterRegistryError) -> CompositionError {
    CompositionError::composition_with_source("Runtime adapter registration failed", error)
}

fn model_runtime_adapter_key(adapter: &model_runtime_binding::Adapter) -> RuntimeAdapterKey {
    match adapter {
        model_runtime_binding::Adapter::SimulatedRuntime(_) => SIMULATED_ADAPTER_KEY,
        model_runtime_binding::Adapter::DockerRuntime(_) => DOCKER_ADAPTER_KEY,
        model_runtime_binding::Adapter::NativeRuntime(_) => NATIVE_ADAPTER_KEY,
    }
}

fn application_adapter_key(adapter: &application_runtime_binding::Adapter) -> RuntimeAdapterKey {
    match adapter {
        application_runtime_binding::Adapter::SimulatedRuntime(_) => SIMULATED_ADAPTER_KEY,
        application_runtime_binding::Adapter::DockerRuntime(_) => DOCKER_ADAPTER_KEY,
    }
}

fn docker_factory(configuration: &SitePaths, site_id: &str) -> DockerRuntimeFactory {
    DockerRuntimeFactory::new(
        site_id.to_owned(),
        configuration.model_artifact_cache_root(),
        configuration.root.join("applications-data"),
        configuration.workload_cache_root(),
    )
}

struct DockerRuntimeAdapterFactory {
    factory: Arc<dyn DockerBackendFactory>,
}

impl ModelRuntimeAdapterFactory for DockerRuntimeAdapterFactory {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ModelRuntimeAdapterFuture<'a> {
        Box::pin(async move {
            let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
                target.runtime_binding.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Model",
                    &target.execution_target_id,
                    "docker",
                ));
            };
            let Some(model_runtime_binding::Adapter::DockerRuntime(settings)) =
                binding.adapter.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Model",
                    &target.execution_target_id,
                    "docker",
                ));
            };
            let transport =
                validated_accelerator_transport(settings, &target.accelerator_device_ids).map_err(
                    |error| {
                        CompositionError::Composition(anyhow::Error::new(error).context(format!(
                            "execution target {:?} has invalid Docker runtime configuration",
                            target.execution_target_id
                        )))
                    },
                )?;
            let node_id = SiteNodeId::parse(&target.site_node_id).map_err(|error| {
                CompositionError::composition_with_source(
                    "execution target has invalid node identity",
                    error,
                )
            })?;
            self.factory
                .model_backend(transport, node_id, target.execution_target_id.clone())
                .await
                .map_err(CompositionError::from)
        })
    }
}

impl ApplicationRuntimeAdapterFactory for DockerRuntimeAdapterFactory {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ApplicationRuntimeAdapterFuture<'a> {
        Box::pin(async move {
            let Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) =
                target.runtime_binding.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Application",
                    &target.execution_target_id,
                    "docker",
                ));
            };
            let Some(application_runtime_binding::Adapter::DockerRuntime(_)) =
                binding.adapter.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Application",
                    &target.execution_target_id,
                    "docker",
                ));
            };
            self.factory
                .application_backend(
                    DockerAcceleratorTransport::Cdi,
                    target.execution_target_id.clone(),
                )
                .await
                .map_err(CompositionError::from)
        })
    }
}

/// Node-scoped factory of native runtime backends, one per native execution target of the node,
/// all keeping their workload state under the site's `<site dir>/runtime/native`.
struct NativeRuntimeAdapterFactory {
    state_root: NativeStateRoot,
    approved_artifact_root: PathBuf,
    site_id: String,
    node_id: SiteNodeId,

    /// Launcher binary resolved when the registry was built, or why none is usable.
    launcher: Result<PathBuf, Arc<NativeLauncherError>>,

    /// Native runtime timing every backend it opens follows.
    timing: NativeTiming,
}

impl NativeRuntimeAdapterFactory {
    fn new(
        configuration: &SitePaths,
        site_id: &str,
        node_id: SiteNodeId,
        launcher: Option<&Path>,
        timing: NativeTiming,
    ) -> Self {
        Self {
            state_root: NativeStateRoot::for_site(&configuration.root),
            approved_artifact_root: configuration.model_artifact_cache_root(),
            site_id: site_id.to_owned(),
            node_id,
            launcher: dotcore_runtime_backends::native::resolve_launcher(launcher)
                .map_err(Arc::new),
            timing,
        }
    }

    /// Opens the native backend of `target`, whose workloads take the lifetime its binding
    /// selects; an ephemeral backend binds its workloads to this server process.
    fn native_backend(
        &self,
        target: &ExecutionTarget,
    ) -> Result<NativeRuntimeBackend, CompositionError> {
        let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
            target.runtime_binding.as_ref()
        else {
            return Err(mismatched_adapter(
                "Model",
                &target.execution_target_id,
                "native",
            ));
        };
        let Some(model_runtime_binding::Adapter::NativeRuntime(settings)) =
            binding.adapter.as_ref()
        else {
            return Err(mismatched_adapter(
                "Model",
                &target.execution_target_id,
                "native",
            ));
        };
        if target.site_node_id != self.node_id.as_str() {
            return Err(CompositionError::composition(format!(
                "native execution target {:?} belongs to node {:?}, not to this node {:?}",
                target.execution_target_id,
                target.site_node_id,
                self.node_id.as_str()
            )));
        }
        let lifetime = native_workload_lifetime(settings.workload_lifetime).ok_or_else(|| {
            CompositionError::composition(format!(
                "native execution target {:?} selects unknown workload lifetime {}",
                target.execution_target_id, settings.workload_lifetime
            ))
        })?;
        let launcher = self.launcher.clone().map_err(|error| {
            CompositionError::Composition(anyhow::Error::new(error).context(format!(
                "native execution target {:?} cannot start workloads without the launcher binary",
                target.execution_target_id
            )))
        })?;
        let config = NativeRuntimeConfig {
            state_root: self.state_root.clone(),
            store_root: PathBuf::from(DEFAULT_STORE_ROOT),
            approved_artifact_root: self.approved_artifact_root.clone(),
            site_id: self.site_id.clone(),
            site_node_id: self.node_id.clone(),
            execution_target_id: target.execution_target_id.clone(),
            lifetime,
        };
        let backend = NativeRuntimeBackend::new(config, launcher, self.timing)
            .map_err(|error| {
                CompositionError::Composition(anyhow::Error::new(error).context(format!(
                    "native execution target {:?} cannot open its workload state under {}; make the site directory writable by the server user",
                    target.execution_target_id,
                    self.state_root.path().display()
                )))
            })?;
        info!(
            event_kind = "native_runtime_composed",
            execution_target_id = %target.execution_target_id,
            lifetime = ?lifetime,
            "composed native runtime backend"
        );
        Ok(backend)
    }
}

impl ModelRuntimeAdapterFactory for NativeRuntimeAdapterFactory {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ModelRuntimeAdapterFuture<'a> {
        Box::pin(async move {
            Ok(Box::new(self.native_backend(target)?) as Box<dyn dotcore_runtime::RuntimeBackend>)
        })
    }
}

struct SimulatedRuntimeAdapterFactory {
    inputs: SimulatedRuntimeInputs,
}

impl ModelRuntimeAdapterFactory for SimulatedRuntimeAdapterFactory {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ModelRuntimeAdapterFuture<'a> {
        Box::pin(async move {
            let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
                target.runtime_binding.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Model",
                    &target.execution_target_id,
                    "simulated",
                ));
            };
            let Some(model_runtime_binding::Adapter::SimulatedRuntime(settings)) =
                binding.adapter.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Model",
                    &target.execution_target_id,
                    "simulated",
                ));
            };
            Ok(Box::new(simulated_runtime(settings, target, &self.inputs)?)
                as Box<dyn dotcore_runtime::RuntimeBackend>)
        })
    }
}

impl ApplicationRuntimeAdapterFactory for SimulatedRuntimeAdapterFactory {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ApplicationRuntimeAdapterFuture<'a> {
        Box::pin(async move {
            let Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) =
                target.runtime_binding.as_ref()
            else {
                return Err(mismatched_adapter(
                    "Application",
                    &target.execution_target_id,
                    "simulated",
                ));
            };
            if !matches!(
                binding.adapter,
                Some(application_runtime_binding::Adapter::SimulatedRuntime(_))
            ) {
                return Err(mismatched_adapter(
                    "Application",
                    &target.execution_target_id,
                    "simulated",
                ));
            }
            let platform = target.platform.as_ref().ok_or_else(|| {
                CompositionError::composition(
                    "simulated application execution target requires a platform",
                )
            })?;
            let execution_platform = dotcore_domain::ExecutionPlatform::try_from(platform)
                .map_err(|error| {
                    CompositionError::composition_with_source(
                        "simulated application execution-target platform is unsupported",
                        error,
                    )
                })?;
            // A representative development site paces application groups like model startup
            // steps, so their transitions last long enough to be observed.
            let group_transition_delay = if self.inputs.representative_startup() {
                self.inputs.timing.startup_step_interval
            } else {
                std::time::Duration::ZERO
            };
            Ok(Box::new(InMemoryRuntimeBackend::for_target(
                SimulatedRuntimeConfig {
                    execution_platform,
                    managed_node_id: dotcore_domain::SiteNodeId::parse(&target.site_node_id).ok(),
                    machine: self.inputs.machine.clone(),
                    group_transition_delay,
                    vary_durations: self.inputs.timing.vary_durations,
                    test_support: self.inputs.test_support.clone(),
                    clock: Arc::clone(&self.inputs.clock),
                    ..Default::default()
                },
                &target.execution_target_id,
            ))
                as Box<dyn dotcore_runtime::WorkloadGroupBackend>)
        })
    }
}

/// Maps the binding's workload lifetime, where an unspecified lifetime means persistent.
fn native_workload_lifetime(value: i32) -> Option<NativeWorkloadLifetime> {
    use dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime as Binding;

    match Binding::try_from(value).ok()? {
        Binding::Unspecified | Binding::Persistent => Some(NativeWorkloadLifetime::Persistent),
        Binding::Ephemeral => Some(NativeWorkloadLifetime::Ephemeral),
    }
}

fn mismatched_adapter(owner: &str, location: &str, expected: &str) -> CompositionError {
    CompositionError::composition(format!(
        "{owner} runtime adapter factory {expected:?} received mismatched configuration at {location:?}"
    ))
}

fn simulated_runtime(
    settings: &dotcore_proto::dotcore::site::v1::SimulatedRuntimeAdapter,
    target: &ExecutionTarget,
    inputs: &SimulatedRuntimeInputs,
) -> Result<InMemoryRuntimeBackend, CompositionError> {
    // A simulated model target has no real hardware to fall back on.
    let machine = inputs.machine.clone().ok_or_else(|| {
        CompositionError::composition(format!(
            "simulated execution target {:?} requires its node to declare a machine",
            target.execution_target_id
        ))
    })?;
    if let Some(device) = target
        .accelerator_device_ids
        .iter()
        .find(|device| machine.description().gpu_vendor(device.as_str()).is_none())
    {
        return Err(CompositionError::composition(format!(
            "simulated execution target {:?} holds GPU {device:?}, which the declared machine {:?} \
             doesn't have",
            target.execution_target_id,
            machine.description().name()
        )));
    }
    let execution_platform = target
        .platform
        .as_ref()
        .ok_or_else(|| {
            CompositionError::composition("simulated execution target requires a platform")
        })
        .and_then(|platform| {
            dotcore_domain::ExecutionPlatform::try_from(platform).map_err(|error| {
                CompositionError::composition_with_source(
                    "simulated execution-target platform is unsupported",
                    error,
                )
            })
        })?;
    let default_ready_delay = settings
        .default_scenario
        .as_ref()
        .map_or(std::time::Duration::ZERO, |scenario| {
            std::time::Duration::from_millis(scenario.ready_delay_millis)
        });
    let default_stop_delay = settings
        .default_scenario
        .as_ref()
        .map_or(std::time::Duration::ZERO, |scenario| {
            std::time::Duration::from_millis(scenario.stop_delay_millis)
        });
    let (deployment_ready_delays, deployment_stop_delays) = settings
        .deployment_scenarios
        .iter()
        .filter_map(|scenario| {
            scenario.scenario.as_ref().map(|value| {
                (
                    (
                        scenario.model_deployment_id.clone(),
                        std::time::Duration::from_millis(value.ready_delay_millis),
                    ),
                    (
                        scenario.model_deployment_id.clone(),
                        std::time::Duration::from_millis(value.stop_delay_millis),
                    ),
                )
            })
        })
        .unzip();
    let startup = inputs
        .representative_startup()
        .then(|| SimulatedStartupConfig::development_default(inputs.timing.startup_step_interval));
    Ok(InMemoryRuntimeBackend::for_target(
        SimulatedRuntimeConfig {
            execution_platform,
            managed_node_id: dotcore_domain::SiteNodeId::parse(&target.site_node_id).ok(),
            machine: Some(machine),
            default_ready_delay,
            default_stop_delay,
            deployment_ready_delays,
            deployment_stop_delays,
            group_transition_delay: std::time::Duration::ZERO,
            vary_durations: inputs.timing.vary_durations,
            startup,
            test_support: inputs.test_support.clone(),
            clock: Arc::clone(&inputs.clock),
        },
        &target.execution_target_id,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use std::time::UNIX_EPOCH;

    use dotcore_domain::{
        clock::{SystemClock, VirtualClock},
        timing::Timing,
    };
    use dotcore_launcher::workload::Lifetime;
    use dotcore_proto::dotcore::{
        catalog::v1::OciPlatform,
        site::v1::{
            ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget,
            ModelRuntimeBinding, NativeRuntimeAdapter,
            NativeWorkloadLifetime as NativeWorkloadLifetimeBinding,
            SimulatedApplicationRuntimeAdapter, SimulatedRuntimeAdapter, SimulatedRuntimeScenario,
            Site, SiteNode, application_runtime_binding, execution_target, model_runtime_binding,
        },
    };
    use dotcore_runtime::{
        RuntimeBackend, WorkloadCorrelation, WorkloadGroupBackend, WorkloadGroupId,
        WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadPublicationRequest,
        WorkloadUnitName,
    };
    use dotcore_runtime_backends::docker::{
        DockerAcceleratorTransport, DockerBackendFactory, DockerBackendFuture,
    };
    use dotcore_site::view::fencing::{
        AcceptedNodeViewAuthority, ActionAssignment, LifecycleOperationAttempt, ManagerSession,
        NodeIncarnation, NodeViewProjection,
    };

    use super::*;
    use crate::dispatch::application::ApplicationRuntimeError;

    #[tokio::test]
    async fn composes_only_the_application_targets_among_the_node_targets() {
        let site = site_with_targets();
        let registry = runtime_adapter_registry_with_docker_factory(
            Arc::new(PanicDockerFactory),
            simulated_inputs(None),
        )
        .expect("registry");
        let applications = node_application_runtimes_for(
            &site.execution_targets,
            &registry,
            view_authority(),
            reconnect_on_system_time(),
        )
        .await
        .expect("Application targets");
        assert_eq!(
            applications.target_ids().collect::<Vec<_>>(),
            ["application-a", "application-b"]
        );
    }

    #[tokio::test]
    async fn an_unreachable_docker_application_target_stays_declared_and_reports_the_outage() {
        let daemon = Arc::new(UnreachableDockerDaemon::default());
        let registry =
            runtime_adapter_registry_with_docker_factory(daemon.clone(), simulated_inputs(None))
                .expect("runtime adapter registry");

        let runtimes = node_application_runtimes_for(
            &[
                simulated_application_target("application-a"),
                docker_application_target("docker-applications"),
            ],
            &registry,
            view_authority(),
            reconnect_on_system_time(),
        )
        .await
        .expect("node composed with an unreachable Docker application target");
        assert_eq!(
            runtimes.target_ids().collect::<Vec<_>>(),
            ["application-a", "docker-applications"],
            "the unreachable target stays declared in the node's application runtimes"
        );
        assert_eq!(
            runtimes
                .backend_id("docker-applications")
                .expect("declared Docker runtime")
                .0,
            dotcore_runtime_backends::docker::BACKEND_ID,
            "the unreachable target keeps reporting the Docker runtime it declares"
        );
        assert_eq!(
            runtimes
                .backend_id("application-a")
                .expect("reachable application runtime")
                .0,
            dotcore_runtime_backends::simulated::BACKEND_ID,
            "the Docker outage replaces no other target's runtime"
        );
        assert_eq!(
            daemon.application_build_attempts(),
            1,
            "composition reached once for the Docker daemon"
        );

        // An operation on the unavailable target opens the connection again and reports the outage.
        let error = runtimes
            .inspect_workload_group("docker-applications", &group_launch("docker-applications"))
            .await
            .expect_err("an unreachable Docker daemon reports its outage");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            "Application execution target \"docker-applications\" runtime failed: Docker runtime failure: docker daemon is not reachable: Docker runtime failure: docker daemon is not reachable: docker daemon is not reachable"
        );
        assert_eq!(
            daemon.application_build_attempts(),
            2,
            "the operation tried the Docker daemon again"
        );
    }

    #[tokio::test]
    async fn a_deferred_docker_application_target_serves_again_once_its_daemon_answers() {
        // The daemon answers from its third attempt on: composition and the first operation reach
        // an outage, and recovery needs no restart, no re-scan and no new composition.
        let daemon = Arc::new(UnreachableDockerDaemon::default().answers_from_attempt(3));
        let registry =
            runtime_adapter_registry_with_docker_factory(daemon.clone(), simulated_inputs(None))
                .expect("runtime adapter registry");
        let clock = Arc::new(VirtualClock::starting_at(UNIX_EPOCH));
        let runtimes = node_application_runtimes_for(
            &[docker_application_target("docker-applications")],
            &registry,
            view_authority(),
            reconnect_on(&clock),
        )
        .await
        .expect("node composed while its Docker daemon is unreachable");

        runtimes
            .inspect_workload_group("docker-applications", &group_launch("docker-applications"))
            .await
            .expect_err("the target is unavailable while the daemon is down");
        clock.advance(Timing::test().application.docker_reconnect_interval);
        assert!(matches!(
            runtimes
                .inspect_workload_group("docker-applications", &group_launch("docker-applications"))
                .await
                .expect("the target recovered with its daemon"),
            WorkloadGroupObservation::Absent { .. }
        ));
        assert_eq!(
            daemon.application_build_attempts(),
            3,
            "one reconnect served again"
        );

        runtimes
            .inspect_workload_group("docker-applications", &group_launch("docker-applications"))
            .await
            .expect("the recovered connection is kept");
        assert_eq!(
            daemon.application_build_attempts(),
            3,
            "a recovered target stops reopening its connection"
        );
    }

    #[tokio::test]
    async fn calls_within_the_retry_window_fail_fast_and_a_call_after_it_reconnects() {
        // The daemon answers from its fourth attempt: composition is attempt one.
        let daemon = Arc::new(UnreachableDockerDaemon::default().answers_from_attempt(4));
        let registry =
            runtime_adapter_registry_with_docker_factory(daemon.clone(), simulated_inputs(None))
                .expect("runtime adapter registry");
        let clock = Arc::new(VirtualClock::starting_at(UNIX_EPOCH));
        let interval = Timing::test().application.docker_reconnect_interval;
        let runtimes = node_application_runtimes_for(
            &[docker_application_target("docker-applications")],
            &registry,
            view_authority(),
            reconnect_on(&clock),
        )
        .await
        .expect("node composed while its Docker daemon is unreachable");
        let launch = group_launch("docker-applications");

        let first = runtimes
            .inspect_workload_group("docker-applications", &launch)
            .await
            .expect_err("the first call reaches the outage");
        let outage = format!("{:#}", anyhow::Error::from(first));
        assert_eq!(
            outage,
            "Application execution target \"docker-applications\" runtime failed: Docker runtime failure: docker daemon is not reachable: Docker runtime failure: docker daemon is not reachable: docker daemon is not reachable",
        );
        assert_eq!(daemon.application_build_attempts(), 2);

        // Within the window, calls report the same outage without reconnecting.
        clock.advance(interval - Duration::from_millis(1));
        for _ in 0..3 {
            let error = runtimes
                .inspect_workload_group("docker-applications", &launch)
                .await
                .expect_err("the outage is reported inside the retry window");
            assert_eq!(format!("{:#}", anyhow::Error::from(error)), outage);
        }
        assert_eq!(
            daemon.application_build_attempts(),
            2,
            "calls inside the window do not reconnect"
        );

        // After the window, one call reconnects; it fails again and opens a new window.
        clock.advance(Duration::from_millis(1));
        runtimes
            .inspect_workload_group("docker-applications", &launch)
            .await
            .expect_err("the daemon is still down");
        assert_eq!(daemon.application_build_attempts(), 3);
        runtimes
            .inspect_workload_group("docker-applications", &launch)
            .await
            .expect_err("a failed reconnect opens a new window");
        assert_eq!(daemon.application_build_attempts(), 3);

        // After the next window the daemon answers and the target serves again.
        clock.advance(interval);
        assert!(matches!(
            runtimes
                .inspect_workload_group("docker-applications", &launch)
                .await
                .expect("the target recovered with its daemon"),
            WorkloadGroupObservation::Absent { .. }
        ));
        assert_eq!(daemon.application_build_attempts(), 4);
    }

    #[tokio::test]
    async fn an_unreachable_docker_application_target_still_acts_under_the_node_view_lease() {
        let daemon = Arc::new(UnreachableDockerDaemon::default());
        let registry =
            runtime_adapter_registry_with_docker_factory(daemon.clone(), simulated_inputs(None))
                .expect("runtime adapter registry");
        let target = docker_application_target("docker-applications");
        let assignment = application_assignment("docker-applications");
        let view = view_authority();
        view.install_for_test(
            "site-one",
            "manager-node",
            1,
            &NodeViewProjection::new(vec![target.clone()], Vec::new())
                .expect("projection")
                .with_application_assignments(vec![assignment.clone()])
                .expect("application assignments"),
        )
        .expect("installed node view");

        let runtimes =
            node_application_runtimes_for(&[target], &registry, view, reconnect_on_system_time())
                .await
                .expect("node composed with an unavailable Docker application target");
        let token = runtimes
            .capture_operation(
                &ActionAssignment::from(&assignment),
                LifecycleOperationAttempt::Runtime {
                    reconcile_generation: 1,
                },
            )
            .expect("the node's action fence still admits the target");
        let error = runtimes
            .create_workload_group(
                &token,
                "docker-applications",
                &group_launch("docker-applications"),
            )
            .await
            .expect_err("the action fails on the daemon, not on the node's fence");
        assert!(
            matches!(
                &error,
                ApplicationRuntimeError::TargetAdapter {
                    execution_target_id,
                    source: RuntimeBackendError::Docker(_),
                } if execution_target_id == "docker-applications"
            ),
            "an admitted action reaches the runtime: {error:?}"
        );
    }

    #[tokio::test]
    async fn an_application_target_without_a_runtime_adapter_still_fails_the_node() {
        let registry = runtime_adapter_registry_with_docker_factory(
            Arc::new(PanicDockerFactory),
            simulated_inputs(None),
        )
        .expect("runtime adapter registry");
        let target = ExecutionTarget {
            execution_target_id: "application-a".to_owned(),
            site_node_id: "manager-node".to_owned(),
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding { adapter: None },
            )),
            ..Default::default()
        };

        let error = node_application_runtimes_for(
            &[target],
            &registry,
            view_authority(),
            reconnect_on_system_time(),
        )
        .await
        .err()
        .expect("an adapter-less application target is invalid configuration");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            "Backend composition failed: Application execution target requires one runtime adapter"
        );
    }

    #[tokio::test]
    async fn native_targets_open_backends_with_their_binding_lifetime() {
        let site_root = crate::test_support::canonical_tempdir().expect("Site root");
        let paths = site_paths(site_root.path());
        let launcher = dotcore_runtime_backends::native::test_support::launcher_binary();
        let registry = runtime_adapter_registry_for_site_id(
            &paths,
            "site-one",
            &node_id(),
            Some(&launcher),
            Timing::test().native,
            simulated_inputs(None),
            None,
        )
        .expect("registry");

        let targets = [
            native_target("mac-persistent", NativeWorkloadLifetimeBinding::Unspecified),
            native_target("mac-ephemeral", NativeWorkloadLifetimeBinding::Ephemeral),
        ];
        let runtimes = node_runtime_targets_for(&targets, &registry, view_authority())
            .await
            .expect("native runtime targets");
        assert_eq!(
            runtimes.target_ids().collect::<Vec<_>>(),
            ["mac-ephemeral", "mac-persistent"]
        );
        assert!(
            site_root
                .path()
                .join("runtime/native/mac-ephemeral")
                .is_dir()
        );

        let factory = NativeRuntimeAdapterFactory::new(
            &paths,
            "site-one",
            node_id(),
            Some(&launcher),
            Timing::test().native,
        );
        let persistent = factory
            .native_backend(&targets[0])
            .expect("persistent backend");
        assert_eq!(persistent.lifetime(), Lifetime::Independent);
        let ephemeral = factory
            .native_backend(&targets[1])
            .expect("ephemeral backend");
        let pid = std::process::id();
        assert_eq!(
            ephemeral.lifetime(),
            Lifetime::Watch {
                pid,
                start_token: dotcore_host_process::start_token(pid)
                    .expect("start token")
                    .expect("current process"),
            }
        );
    }

    #[tokio::test]
    async fn missing_launcher_fails_only_nodes_with_native_targets() {
        let site_root = crate::test_support::canonical_tempdir().expect("Site root");
        let missing = site_root.path().join("bin/dotcore-launcher");
        let registry = runtime_adapter_registry_for_site_id(
            &site_paths(site_root.path()),
            "site-one",
            &node_id(),
            Some(&missing),
            Timing::test().native,
            simulated_inputs(Some(rtx_5090_desktop())),
            None,
        )
        .expect("registry without a launcher");

        let simulated = site_with_targets().execution_targets;
        node_runtime_targets_for(&simulated, &registry, view_authority())
            .await
            .expect("node without native targets");

        let error = match node_runtime_targets_for(
            &[native_target(
                "mac-gpu",
                NativeWorkloadLifetimeBinding::Persistent,
            )],
            &registry,
            view_authority(),
        )
        .await
        {
            Ok(_) => panic!("native target composed without a launcher"),
            Err(error) => error,
        };
        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            format!(
                "Backend composition failed: native execution target \"mac-gpu\" cannot start workloads without the launcher binary: launcher binary {} is missing; install dotcore-launcher beside the server executable, or build it with `cargo build -p dotcore-launcher` in development",
                missing.display()
            )
        );
    }

    #[tokio::test]
    async fn simulated_model_targets_compose_only_on_a_declared_machine_holding_their_gpus() {
        let compose = |machine: Option<DeclaredMachine>, gpu: &str| {
            let mut target = site_with_targets().execution_targets.remove(0);
            target.accelerator_device_ids = vec![gpu.to_owned()];
            async move {
                let registry = runtime_adapter_registry_with_docker_factory(
                    Arc::new(PanicDockerFactory),
                    simulated_inputs(machine),
                )
                .expect("registry");
                node_runtime_targets_for(&[target], &registry, view_authority())
                    .await
                    .map(|targets| targets.target_ids().map(str::to_owned).collect::<Vec<_>>())
                    .map_err(|error| format!("{:#}", anyhow::Error::from(error)))
            }
        };

        assert_eq!(
            compose(Some(rtx_5090_desktop()), RTX_5090_GPU_ID).await,
            Ok(vec!["model-a".to_owned()])
        );
        assert_eq!(
            compose(None, RTX_5090_GPU_ID).await,
            Err(
                "Backend composition failed: simulated execution target \"model-a\" requires its \
                 node to declare a machine"
                    .to_owned()
            )
        );
        assert_eq!(
            compose(Some(rtx_5090_desktop()), "GPU-other").await,
            Err(
                "Backend composition failed: simulated execution target \"model-a\" holds GPU \
                 \"GPU-other\", which the declared machine \"simulated-rtx-5090\" doesn't have"
                    .to_owned()
            )
        );
    }

    #[test]
    fn unwritable_native_state_root_fails_the_target_with_its_location() {
        let site_root = crate::test_support::canonical_tempdir().expect("Site root");
        // A file where the runtime directory belongs makes the state root uncreatable.
        std::fs::write(site_root.path().join("runtime"), b"").expect("blocking file");
        let factory = NativeRuntimeAdapterFactory::new(
            &site_paths(site_root.path()),
            "site-one",
            node_id(),
            Some(&dotcore_runtime_backends::native::test_support::launcher_binary()),
            Timing::test().native,
        );

        let error = match factory.native_backend(&native_target(
            "mac-gpu",
            NativeWorkloadLifetimeBinding::Persistent,
        )) {
            Ok(_) => panic!("native backend opened over a file"),
            Err(error) => error,
        };

        let CompositionError::Composition(error) = error else {
            panic!("unexpected composition error {error:?}");
        };
        assert_eq!(
            error.to_string(),
            format!(
                "native execution target \"mac-gpu\" cannot open its workload state under {}; make the site directory writable by the server user",
                site_root.path().join("runtime/native").display()
            )
        );
    }

    fn view_authority() -> AcceptedNodeViewAuthority {
        AcceptedNodeViewAuthority::new(
            "site-one",
            "manager-node",
            NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("session"),
            std::time::Duration::from_secs(30),
        )
        .expect("view authority")
    }

    fn site_with_targets() -> Site {
        let model = |id: &str| ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(platform()),
            accelerator_device_ids: vec![RTX_5090_GPU_ID.to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        simulated_runtime(),
                    )),
                },
            )),
        };
        let application = |id: &str| ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(platform()),
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                        SimulatedApplicationRuntimeAdapter {},
                    )),
                },
            )),
        };
        Site {
            nodes: vec![SiteNode {
                node_id: "manager-node".to_owned(),
                display_name: "Manager".to_owned(),
                host: "127.0.0.1".to_owned(),
                ..Default::default()
            }],
            execution_targets: vec![
                model("model-a"),
                model("model-b"),
                application("application-a"),
                application("application-b"),
            ],
            ..Default::default()
        }
    }

    fn native_target(id: &str, lifetime: NativeWorkloadLifetimeBinding) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "darwin".to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                        NativeRuntimeAdapter {
                            workload_lifetime: lifetime as i32,
                        },
                    )),
                },
            )),
        }
    }

    fn simulated_application_target(execution_target_id: &str) -> ExecutionTarget {
        application_target(
            execution_target_id,
            application_runtime_binding::Adapter::SimulatedRuntime(
                SimulatedApplicationRuntimeAdapter {},
            ),
        )
    }

    fn docker_application_target(execution_target_id: &str) -> ExecutionTarget {
        application_target(
            execution_target_id,
            application_runtime_binding::Adapter::DockerRuntime(DockerApplicationRuntimeAdapter {}),
        )
    }

    fn application_target(
        execution_target_id: &str,
        adapter: application_runtime_binding::Adapter,
    ) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: execution_target_id.to_owned(),
            display_name: execution_target_id.to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(platform()),
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(adapter),
                },
            )),
        }
    }

    /// One application assignment placing a deployment on `execution_target_id` of this node, for
    /// the runtime actions the node's fence admits.
    fn application_assignment(
        execution_target_id: &str,
    ) -> dotcore_proto::dotcore::application::private::v1::ApplicationAssignment {
        dotcore_proto::dotcore::application::private::v1::ApplicationAssignment {
            declaration: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration {
                    application_deployment_id: "application-01".to_owned(),
                    application_deployment_name: "application-01".to_owned(),
                    placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                        site_node_id: "manager-node".to_owned(),
                        execution_target_id: execution_target_id.to_owned(),
                    }),
                    lifecycle_generation: 1,
                    ..Default::default()
                },
            ),
            linked_route: None,
        }
    }

    fn group_launch(execution_target_id: &str) -> WorkloadGroupLaunch {
        WorkloadGroupLaunch {
            group_id: WorkloadGroupId("application-01".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "application-01".to_owned(),
                reconcile_generation: 1,
            },
            execution_target: dotcore_runtime::ExecutionTarget {
                managed_node_id: SiteNodeId::for_test("manager-node"),
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

    /// GPU of the named `rtx-5090` machine.
    const RTX_5090_GPU_ID: &str = "GPU-00000000-0000-0000-0000-000000005090";

    fn rtx_5090_desktop() -> DeclaredMachine {
        DeclaredMachine::new(
            dotcore_node_hardware::MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
                .expect("named machine"),
            Arc::new(SystemClock),
        )
    }

    fn site_paths(root: &std::path::Path) -> SitePaths {
        SitePaths {
            root: root.to_path_buf(),
            master_catalog_root: None,
            cache_root: root.join("cache"),
        }
    }

    fn node_id() -> SiteNodeId {
        SiteNodeId::parse("manager-node").expect("node id")
    }

    fn simulated_runtime() -> SimulatedRuntimeAdapter {
        SimulatedRuntimeAdapter {
            default_scenario: Some(SimulatedRuntimeScenario::default()),
            deployment_scenarios: Vec::new(),
        }
    }

    fn platform() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    }

    struct PanicDockerFactory;

    impl DockerBackendFactory for PanicDockerFactory {
        fn model_backend(
            &self,
            _transport: DockerAcceleratorTransport,
            _node: dotcore_domain::SiteNodeId,
            _target: String,
        ) -> DockerBackendFuture<'_, Box<dyn RuntimeBackend>> {
            Box::pin(async { panic!("Docker factory must not be called") })
        }

        fn application_backend(
            &self,
            _transport: DockerAcceleratorTransport,
            _target: String,
        ) -> DockerBackendFuture<'_, Box<dyn WorkloadGroupBackend>> {
            Box::pin(async { panic!("Docker factory must not be called") })
        }
    }

    /// The Docker daemon of a node whose daemon cannot be reached. By default it never answers;
    /// `answers_from_attempt` makes that attempt and every later one reach a working Docker
    /// backend, while earlier ones report the outage.
    #[derive(Default)]
    struct UnreachableDockerDaemon {
        application_attempts: AtomicUsize,
        answers_from: Option<usize>,
    }

    impl UnreachableDockerDaemon {
        /// Answers from attempt `attempt` on, counting attempts from one.
        fn answers_from_attempt(mut self, attempt: usize) -> Self {
            self.answers_from = Some(attempt);
            self
        }

        fn application_build_attempts(&self) -> usize {
            self.application_attempts.load(Ordering::SeqCst)
        }
    }

    impl DockerBackendFactory for UnreachableDockerDaemon {
        fn model_backend(
            &self,
            _transport: DockerAcceleratorTransport,
            _node: dotcore_domain::SiteNodeId,
            _target: String,
        ) -> DockerBackendFuture<'_, Box<dyn RuntimeBackend>> {
            Box::pin(async { panic!("these tests compose no Docker model target") })
        }

        fn application_backend(
            &self,
            _transport: DockerAcceleratorTransport,
            execution_target_id: String,
        ) -> DockerBackendFuture<'_, Box<dyn WorkloadGroupBackend>> {
            let attempt = self.application_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                if !self
                    .answers_from
                    .is_some_and(|answers_from| attempt >= answers_from)
                {
                    return Err(RuntimeBackendError::Docker(anyhow::anyhow!(
                        "docker daemon is not reachable"
                    )));
                }
                Ok(Box::new(InMemoryRuntimeBackend::for_target(
                    SimulatedRuntimeConfig {
                        execution_platform: ExecutionPlatform::linux_amd64(),
                        ..Default::default()
                    },
                    &execution_target_id,
                )) as Box<dyn WorkloadGroupBackend>)
            })
        }
    }

    fn reconnect_on(clock: &Arc<VirtualClock>) -> DockerReconnect {
        DockerReconnect {
            clock: clock.clone(),
            interval: Timing::test().application.docker_reconnect_interval,
        }
    }

    fn reconnect_on_system_time() -> DockerReconnect {
        DockerReconnect {
            clock: Arc::new(SystemClock),
            interval: Timing::test().application.docker_reconnect_interval,
        }
    }

    /// Simulated runtime inputs of a test node on `machine`, without test controls, on system
    /// time.
    fn simulated_inputs(machine: Option<DeclaredMachine>) -> SimulatedRuntimeInputs {
        SimulatedRuntimeInputs {
            test_support: None,
            machine,
            timing: Timing::test().simulated,
            clock: Arc::new(SystemClock),
        }
    }
}
