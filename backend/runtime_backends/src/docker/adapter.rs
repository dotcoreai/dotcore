//! Docker implementation of the runtime's owner-neutral workload and workload-group
//! lifecycle contracts: it runs model workload containers on the local Docker host and
//! turns launch requests into container operations.
//!
//! Workload identity comes from the `com.dotcore.*` ownership labels stamped at creation
//! time, so a restarted backend re-adopts its existing containers under the same stable
//! identity, such as `dotcore_<site>_<owner>_<generation>`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use dotcore_domain::ExecutionPlatform;
use dotcore_domain::{HostResolver, SiteNodeId, SystemHostResolver};
use dotcore_runtime::{
    LoopbackEndpoint, ObservedWorkload, ObservedWorkloadGroup, RuntimeBackend, RuntimeBackendError,
    RuntimeBackendId, RuntimeFuture, RuntimeGroupLogFollowQuery, RuntimeLogFuture,
    RuntimeLogHistory, RuntimeLogSourceHistory, RuntimeLogSourceSelection, RuntimeLogSourceStream,
    RuntimeLogStream, RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadCorrelation,
    WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupLaunch, WorkloadGroupObservation,
    WorkloadId, WorkloadLaunch, WorkloadObservation, WorkloadServiceBinding, WorkloadStorageKey,
};

pub(super) use super::configuration::{DockerAcceleratorTransport, DockerRuntimeConfig};
use super::{
    client::{DockerClient, LocalDockerClient},
    request::{DockerPort, PreparedWorkload},
    site_data::remove_folder_tree,
    storage::{materialize_storage, retained_storage_folders, storage_key_path},
};

pub const BACKEND_ID: &str = "runtime/docker";

/// Docker implementation of the runtime's owner-neutral workload and workload-group
/// lifecycle contracts: it runs model workload containers on the local Docker host.
///
/// Site composition and the development runtime each open their own connected backend.
pub struct DockerRuntimeBackend {
    inner: DockerAdapter<LocalDockerClient>,
    execution_platform: ExecutionPlatform,
}

pub(super) struct DockerAdapter<C> {
    pub(super) client: C,
    pub(super) config: DockerRuntimeConfig,
    managed_node_id: Option<SiteNodeId>,
    known: Mutex<BTreeMap<String, KnownWorkload>>,
    pub(super) groups: std::sync::OnceLock<Mutex<BTreeMap<String, super::group::KnownGroup>>>,
    /// Resolves the site host names group containers get as host entries.
    pub(super) host_resolver: Arc<dyn HostResolver>,
}

impl DockerRuntimeBackend {
    pub async fn connect(config: DockerRuntimeConfig) -> Result<Self, RuntimeBackendError> {
        Self::connect_with_node(config, None).await
    }

    pub async fn connect_managed(
        config: DockerRuntimeConfig,
        managed_node_id: SiteNodeId,
    ) -> Result<Self, RuntimeBackendError> {
        Self::connect_with_node(config, Some(managed_node_id)).await
    }

    async fn connect_with_node(
        config: DockerRuntimeConfig,
        managed_node_id: Option<SiteNodeId>,
    ) -> Result<Self, RuntimeBackendError> {
        let docker = LocalDockerClient::connect().map_err(|error| {
            RuntimeBackendError::docker_with_source("Docker client connection failed", error)
        })?;
        let docker = docker.negotiate_version().await.map_err(|error| {
            RuntimeBackendError::docker_with_source("Docker API version negotiation failed", error)
        })?;
        let execution_platform = super::capabilities::probe_connected_daemon(&docker)
            .await
            .map_err(|error| {
                error
                    .downcast_ref::<dotcore_domain::ExecutionPlatformError>()
                    .cloned()
                    .map(RuntimeBackendError::ExecutionPlatform)
                    .unwrap_or_else(|| {
                        RuntimeBackendError::Docker(
                            error.context("Docker daemon capability probe failed"),
                        )
                    })
            })?
            .execution_platform;
        Ok(Self {
            inner: DockerAdapter::new_with_node(docker, config, managed_node_id),
            execution_platform,
        })
    }

    pub async fn connect_development(
        scope: String,
        cache_root: &std::path::Path,
        accelerator_transport: DockerAcceleratorTransport,
    ) -> Result<Self, RuntimeBackendError> {
        let cache = dotcore_domain::CacheLayout(cache_root);
        let application_data_root = cache.development_applications();
        let workload_cache_root = cache.workloads();
        for (root, context) in [
            (
                &application_data_root,
                "Development application data root creation failed",
            ),
            (&workload_cache_root, "Workload cache root creation failed"),
        ] {
            std::fs::create_dir_all(root)
                .map_err(|error| RuntimeBackendError::docker_with_source(context, error))?;
        }
        Self::connect(DockerRuntimeConfig {
            site_id: scope,
            execution_target_id: "development-target".to_owned(),
            approved_mount_root: cache.model_artifacts(),
            application_data_root,
            workload_cache_root,
            accelerator_transport,
            source_stream_permits: super::configuration::DockerLogSourcePermits::new(),
        })
        .await
    }

    /// Counts every Docker container carrying this reserved development scope.
    pub async fn development_namespace_evidence_count(&self) -> Result<usize, RuntimeBackendError> {
        self.inner.development_namespace_evidence_count().await
    }

    /// Creates but does not start one workload for exact development recovery boundaries.
    pub async fn create_development_workload(
        &self,
        workload: &WorkloadLaunch,
    ) -> Result<WorkloadObservation, RuntimeBackendError> {
        self.inner.create_stopped_workload(workload).await
    }

    /// Inspects Docker-native facts for the private development workload facade.
    pub async fn inspect_development_process(
        &self,
        workload_id: &WorkloadId,
    ) -> Result<DockerDevelopmentProcess, RuntimeBackendError> {
        self.inner.inspect_development_process(workload_id).await
    }
}

impl RuntimeBackend for DockerRuntimeBackend {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackend::backend_id(&self.inner)
    }

    fn execution_platform<'a>(&'a self) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(self.execution_platform.clone())))
    }

    fn supports_host_shared_ipc(&self) -> bool {
        RuntimeBackend::supports_host_shared_ipc(&self.inner)
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(self.inner.discover_workloads())
    }

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        if let Err(error) = workload
            .spec
            .validate_runtime_platform(&self.execution_platform)
        {
            return Box::pin(std::future::ready(Err(error)));
        }
        Box::pin(self.inner.create_workload(workload))
    }

    fn start_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.inner.start_workload(workload_id))
    }

    fn inspect_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.inner.inspect_workload(workload_id))
    }

    fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(self.inner.stop_workload(workload_id))
    }

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(self.inner.remove_workload(workload_id))
    }

    fn read_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(self.inner.read_logs(workload_id, query))
    }

    fn follow_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(self.inner.follow_logs(workload_id, query))
    }

    fn read_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        self.inner
            .read_workload_logs_by_correlation(correlation, query)
    }

    fn follow_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        self.inner
            .follow_workload_logs_by_correlation(correlation, query)
    }
}

impl WorkloadGroupBackend for DockerRuntimeBackend {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackend::backend_id(&self.inner)
    }

    fn execution_platform<'a>(
        &'a self,
        _execution_target_id: &'a str,
    ) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(self.execution_platform.clone())))
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        self.inner.discover_workload_groups()
    }

    fn storage_exists<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
        Box::pin(async move {
            match std::fs::symlink_metadata(storage_key_path(&self.inner.config, storage)?) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(RuntimeBackendError::docker_with_source(
                    "Workload storage existence check failed",
                    error,
                )),
            }
        })
    }

    fn remove_storage<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let path = storage_key_path(&self.inner.config, storage)?;
            remove_folder_tree(&path).await.map_err(|error| {
                RuntimeBackendError::Docker(error.context("Workload storage removal failed"))
            })
        })
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        group_id: &'a WorkloadGroupId,
        owner_id: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        self.inner.ensure_workload_group_absent(group_id, owner_id)
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.inner.create_workload_group(group)
    }
    fn start_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.inner.start_workload_group(group)
    }
    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.inner.inspect_workload_group(group)
    }
    fn remove_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        self.inner.remove_workload_group(group)
    }
    fn remove_observed_workload_group<'a>(
        &'a self,
        group: &'a ObservedWorkloadGroup,
    ) -> RuntimeFuture<'a, ()> {
        self.inner.remove_observed_group(group)
    }
    fn stop_workload_group<'a>(&'a self, group: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        self.inner.stop_workload_group(group)
    }

    fn read_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        sources: &'a RuntimeLogSourceSelection,
        query: &'a dotcore_runtime::RuntimeLogQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceHistory>> {
        self.inner.read_workload_group_logs(group, sources, query)
    }

    fn follow_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        sources: &'a RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceStream>> {
        self.inner.follow_workload_group_logs(group, sources, query)
    }
}

impl<C> DockerAdapter<C> {
    #[cfg(test)]
    pub(super) fn new(client: C, config: DockerRuntimeConfig) -> Self {
        Self::new_with_node(client, config, None)
    }

    pub(super) fn new_with_node(
        client: C,
        config: DockerRuntimeConfig,
        managed_node_id: Option<SiteNodeId>,
    ) -> Self {
        Self {
            client,
            config,
            managed_node_id,
            known: Mutex::new(BTreeMap::new()),
            groups: std::sync::OnceLock::new(),
            host_resolver: Arc::new(SystemHostResolver),
        }
    }

    #[cfg(test)]
    pub(super) fn with_host_resolver(mut self, host_resolver: Arc<dyn HostResolver>) -> Self {
        self.host_resolver = host_resolver;
        self
    }
}

impl<C: DockerClient> RuntimeBackend for DockerAdapter<C> {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(BACKEND_ID.to_owned())
    }

    fn supports_host_shared_ipc(&self) -> bool {
        true
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(async move {
            let containers = self
                .client
                .list_managed_containers()
                .await
                .map_err(|error| {
                    RuntimeBackendError::docker_with_source(
                        "Docker owned-container discovery failed",
                        error,
                    )
                })?;
            let mut discovered = BTreeMap::new();
            let mut correlations = BTreeMap::<String, String>::new();
            for container in containers {
                let Some(mut known) =
                    container.classify_owned(&self.config, RuntimeBackend::backend_id(self))?
                else {
                    continue;
                };
                known.managed_node_id.clone_from(&self.managed_node_id);
                let workload_id = known.identity.workload_id.0.clone();
                if discovered
                    .insert(workload_id.clone(), known.clone())
                    .is_some()
                {
                    return Err(RuntimeBackendError::invalid_launch(format!(
                        "Docker ownership state contains duplicate workload {workload_id}"
                    )));
                }
                if let Some(previous) = correlations.insert(
                    known.identity.correlation.owner_id.clone(),
                    workload_id.clone(),
                ) {
                    return Err(RuntimeBackendError::invalid_launch(format!(
                        "Docker ownership state contains duplicate workload correlation {} in workloads {previous} and {workload_id}",
                        known.identity.correlation.owner_id
                    )));
                }
            }
            *self.known.lock().expect("Docker workload registry") = discovered;
            let workload_ids = self
                .known
                .lock()
                .expect("Docker workload registry")
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            let mut observations = Vec::with_capacity(workload_ids.len());
            for workload_id in workload_ids {
                observations.push(
                    self.inspect_known_workload(&WorkloadId(workload_id))
                        .await?,
                );
            }
            Ok(observations)
        })
    }

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(async move {
            let observation = self.create_stopped_workload(workload).await?;
            match observation {
                WorkloadObservation::Stopped {
                    workload: observed, ..
                } => self.start_workload(&observed.workload_id).await,
                observation => Ok(observation),
            }
        })
    }

    fn start_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(async move {
            let known = self.live_known_workload(workload_id).await?;
            let inspection = self
                .client
                .inspect_container(&known.container_id)
                .await
                .map_err(|error| {
                    RuntimeBackendError::docker_with_source(
                        format!(
                            "Docker container mount inspection failed for workload {}",
                            known.container_id
                        ),
                        error,
                    )
                })?;
            materialize_storage(
                &self.config,
                retained_storage_folders(&self.config, &inspection.bind_sources)?,
            )?;
            self.client
                .start_container(&known.container_id)
                .await
                .map_err(|error| {
                    RuntimeBackendError::docker_with_source(
                        format!(
                            "Docker container start failed for workload {}",
                            known.container_id
                        ),
                        error,
                    )
                })?;
            self.inspect_known_workload(workload_id).await
        })
    }

    fn inspect_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.inspect_known_workload(workload_id))
    }

    fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let known = self.live_known_workload(workload_id).await?;
            self.client
                .stop_container(&known.container_id)
                .await
                .or_else(|error| {
                    if error.is_not_found() || error.is_not_modified() {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| {
                    RuntimeBackendError::docker_with_source(
                        format!(
                            "Docker container stop failed for workload {}",
                            known.container_id
                        ),
                        error,
                    )
                })
        })
    }

    fn read_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(self.read_logs(workload_id, query))
    }

    fn follow_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(self.follow_logs(workload_id, query))
    }

    fn read_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(async move {
            let workload_id = self.workload_id_for_correlation(correlation)?;
            self.read_logs(&workload_id, query).await
        })
    }

    fn follow_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(async move {
            let workload_id = self.workload_id_for_correlation(correlation)?;
            self.follow_logs(&workload_id, query).await
        })
    }

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let known = self.live_known_workload(workload_id).await?;
            if let Err(error) = self.client.remove_container(&known.container_id).await
                && !error.is_not_found()
            {
                return Err(RuntimeBackendError::docker_with_source(
                    format!(
                        "Docker container removal failed for workload {}",
                        known.container_id
                    ),
                    error,
                ));
            }
            self.known
                .lock()
                .expect("Docker workload registry")
                .remove(&workload_id.0);
            Ok(())
        })
    }
}

impl<C: DockerClient> DockerAdapter<C> {
    async fn create_stopped_workload(
        &self,
        workload: &WorkloadLaunch,
    ) -> Result<WorkloadObservation, RuntimeBackendError> {
        let prepared = PreparedWorkload::from_launch(
            workload,
            &self.config,
            RuntimeBackend::backend_id(self),
        )?;
        if self
            .known
            .lock()
            .expect("Docker workload registry")
            .contains_key(&prepared.request.name)
        {
            return self
                .inspect_known_workload(&prepared.identity.workload_id)
                .await;
        }
        if self
            .client
            .container_exists(&prepared.request.name)
            .await
            .map_err(|error| {
                RuntimeBackendError::docker_with_source(
                    format!(
                        "Docker container name preflight failed for workload {}",
                        prepared.request.name
                    ),
                    error,
                )
            })?
        {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload name {} is already in use by an unowned container",
                prepared.request.name
            )));
        }
        materialize_storage(&self.config, workload.spec.storage.iter().map(Into::into))?;
        let container_id = self
            .client
            .create_container(&prepared.request)
            .await
            .map_err(|error| {
                RuntimeBackendError::docker_with_source(
                    format!(
                        "Docker container creation failed for workload {}",
                        prepared.request.name
                    ),
                    error,
                )
            })?;
        self.known.lock().expect("Docker workload registry").insert(
            prepared.request.name.clone(),
            KnownWorkload {
                container_id,
                identity: prepared.identity.clone(),
                ports: prepared.request.ports,
                launch_fingerprint: workload.exact_intent_fingerprint(),
                managed_node_id: Some(prepared.managed_node_id),
            },
        );
        Ok(WorkloadObservation::stopped(prepared.identity))
    }

    #[cfg(test)]
    async fn ensure_development_namespace_empty(&self) -> Result<(), RuntimeBackendError> {
        if self.development_namespace_evidence_count().await? != 0 {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker runtime rejected existing ownership evidence in the reserved development namespace",
            ));
        }
        Ok(())
    }

    async fn development_namespace_evidence_count(&self) -> Result<usize, RuntimeBackendError> {
        self.client
            .list_site_containers(&self.config.site_id)
            .await
            .map(|containers| containers.len())
            .map_err(|error| {
                RuntimeBackendError::docker_with_source(
                    "Docker reserved development namespace inspection failed",
                    error,
                )
            })
    }

    fn workload_id_for_correlation(
        &self,
        correlation: &WorkloadCorrelation,
    ) -> Result<WorkloadId, RuntimeBackendError> {
        let matches = self
            .known
            .lock()
            .expect("Docker workload registry")
            .values()
            .filter(|known| known.identity.correlation == *correlation)
            .map(|known| known.identity.workload_id.clone())
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [workload_id] => Ok(workload_id.clone()),
            [] => Err(RuntimeBackendError::workload_not_created()),
            _ => Err(RuntimeBackendError::invalid_launch(
                "Docker has ambiguous correlated workloads",
            )),
        }
    }

    async fn inspect_development_process(
        &self,
        workload_id: &WorkloadId,
    ) -> Result<DockerDevelopmentProcess, RuntimeBackendError> {
        let known = self.live_known_workload(workload_id).await?;
        let inspection = self
            .client
            .inspect_container(&known.container_id)
            .await
            .map_err(|error| {
                RuntimeBackendError::docker_with_source(
                    format!(
                        "Docker container inspection failed for workload {}",
                        known.container_id
                    ),
                    error,
                )
            })?;
        if inspection.running && inspection.process_id.is_none() {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker development workload has no running process identity",
            ));
        }
        let loopback_endpoints = if inspection.running {
            loopback_endpoints(&known, &inspection)?
        } else {
            Vec::new()
        };
        Ok(DockerDevelopmentProcess {
            container_id: known.container_id,
            process_id: inspection.process_id,
            launch_fingerprint: known.launch_fingerprint,
            loopback_endpoints,
        })
    }

    async fn inspect_known_workload(
        &self,
        workload_id: &WorkloadId,
    ) -> Result<WorkloadObservation, RuntimeBackendError> {
        let known = self.live_known_workload(workload_id).await?;
        let inspection = match self.client.inspect_container(&known.container_id).await {
            Ok(inspection) => inspection,
            Err(error) if error.is_not_found() => {
                self.forget_workload(workload_id);
                return Ok(WorkloadObservation::Absent(known.identity));
            }
            Err(error) => {
                return Err(RuntimeBackendError::docker_with_source(
                    format!(
                        "Docker container inspection failed for workload {}",
                        known.container_id
                    ),
                    error,
                ));
            }
        };
        if !inspection.running {
            return Ok(WorkloadObservation::Stopped {
                workload: known.identity,
                exit_status: inspection.exit_status,
            });
        }
        let loopback_endpoints = loopback_endpoints(&known, &inspection)?;
        let mut identity = known.identity;
        identity.loopback_endpoints = loopback_endpoints;
        identity.services = identity
            .loopback_endpoints
            .iter()
            .cloned()
            .map(|endpoint| {
                WorkloadServiceBinding::loopback(
                    identity.workload_id.0.clone(),
                    endpoint.port_name.clone(),
                    u64::from(endpoint.host_port),
                    endpoint,
                )
            })
            .collect();
        identity.host_process = known.managed_node_id.and_then(|managed_node_id| {
            inspection
                .process_id
                .and_then(|process_id| container_host_process(managed_node_id, process_id))
        });
        Ok(WorkloadObservation::Running(identity))
    }

    fn known_workload(
        &self,
        workload_id: &WorkloadId,
    ) -> Result<KnownWorkload, RuntimeBackendError> {
        self.known
            .lock()
            .expect("Docker workload registry")
            .get(&workload_id.0)
            .cloned()
            .ok_or_else(|| {
                RuntimeBackendError::invalid_launch(format!(
                    "Docker workload {} is unknown",
                    workload_id.0
                ))
            })
    }

    pub(super) async fn live_known_workload(
        &self,
        workload_id: &WorkloadId,
    ) -> Result<KnownWorkload, RuntimeBackendError> {
        let known = self.known_workload(workload_id)?;
        let container = self
            .client
            .inspect_owned_container(&known.container_id)
            .await
            .map_err(|error| {
                RuntimeBackendError::docker_with_source(
                    format!(
                        "Docker container ownership inspection failed for workload {}",
                        known.container_id
                    ),
                    error,
                )
            })?;
        let live = container
            .classify_owned(&self.config, RuntimeBackend::backend_id(self))?
            .ok_or_else(|| {
                RuntimeBackendError::invalid_launch(format!(
                    "Docker workload {} no longer has local Dotcore ownership labels",
                    workload_id.0
                ))
            })?;
        if live.container_id != known.container_id
            || live.identity.workload_id != known.identity.workload_id
            || live.identity.correlation != known.identity.correlation
            || live.identity.backend_id != known.identity.backend_id
            || live.ports != known.ports
        {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} ownership changed after discovery",
                workload_id.0
            )));
        }
        Ok(known)
    }

    fn forget_workload(&self, workload_id: &WorkloadId) {
        self.known
            .lock()
            .expect("Docker workload registry")
            .remove(&workload_id.0);
    }
}

fn loopback_endpoints(
    known: &KnownWorkload,
    inspection: &super::client::ContainerInspection,
) -> Result<Vec<LoopbackEndpoint>, RuntimeBackendError> {
    known
        .ports
        .iter()
        .map(|port| {
            let host_port = inspection
                .host_ports
                .get(&port.container_port)
                .copied()
                .ok_or_else(|| {
                    RuntimeBackendError::invalid_launch(format!(
                        "Docker workload {} has no loopback port mapping for declared port {}",
                        known.container_id, port.name
                    ))
                })?;
            if host_port == 0 {
                return Err(RuntimeBackendError::invalid_launch(format!(
                    "Docker workload {} has invalid zero loopback host port for declared port {}",
                    known.container_id, port.name
                )));
            }
            Ok(LoopbackEndpoint {
                port_name: port.name.clone(),
                host_port,
            })
        })
        .collect()
}

/// Returns the host identity of a running container's main process, or `None` when it is
/// unavailable.
///
/// On macOS, Docker runs containers in a Linux virtual machine, so a container PID names a process
/// of that machine rather than of the Mac host and must never be read from the host table.
pub(super) fn container_host_process(
    managed_node_id: SiteNodeId,
    process_id: u32,
) -> Option<dotcore_domain::host_process::HostProcessIdentity> {
    if cfg!(target_os = "macos") {
        return None;
    }
    dotcore_host_process::read_process_start_token(process_id)
        .ok()
        .map(
            |process_start_token| dotcore_domain::host_process::HostProcessIdentity {
                managed_node_id,
                process_id,
                process_start_token,
            },
        )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerDevelopmentProcess {
    pub container_id: String,
    pub process_id: Option<u32>,
    pub launch_fingerprint: String,
    pub loopback_endpoints: Vec<LoopbackEndpoint>,
}

#[derive(Clone)]
pub(super) struct KnownWorkload {
    pub(super) container_id: String,
    pub(super) identity: ObservedWorkload,
    pub(super) ports: Vec<DockerPort>,
    pub(super) launch_fingerprint: String,
    pub(super) managed_node_id: Option<SiteNodeId>,
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        future::ready,
        sync::{Arc, Mutex},
    };

    use anyhow::anyhow;
    use dotcore_runtime::{
        RuntimeLogQuery, RuntimeLogSource, RuntimeWorkloadLogQuery, WorkloadArtifactBinding,
        WorkloadCorrelation, WorkloadExecutable, WorkloadExecutableArtifact, WorkloadPort,
        WorkloadSpec, WorkloadValue,
    };

    use super::super::{
        client::{
            ClientError, ContainerInspection, DockerContainer, DockerLogOutput, DockerLogRequest,
            DockerLogStream,
        },
        ownership::{
            LABEL_CORRELATION, LABEL_EXECUTION_TARGET, LABEL_GENERATION, LABEL_LAUNCH_FINGERPRINT,
            LABEL_MANAGED, LABEL_PORTS, LABEL_SCHEMA, LABEL_SITE, LABEL_WORKLOAD, OWNERSHIP_SCHEMA,
        },
        request::DockerCreateRequest,
    };
    use super::*;

    #[test]
    fn runtime_owns_lifecycle_and_never_pulls_images() {
        let runtime_source = include_str!("adapter.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("Docker runtime production source");

        assert!(!runtime_source.contains("fn pull("));
        assert!(!runtime_source.contains("create_image"));
    }

    #[tokio::test]
    async fn storage_removal_deletes_only_the_folder_of_its_key() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("data root");
        let data_root = directory.path().to_path_buf();
        // The Docker client connects lazily; data removal never reaches the daemon here.
        let backend = DockerRuntimeBackend {
            inner: DockerAdapter::new(
                LocalDockerClient::connect().expect("Docker client"),
                DockerRuntimeConfig {
                    application_data_root: data_root.clone(),
                    ..config()
                },
            ),
            execution_platform: ExecutionPlatform::linux_amd64(),
        };
        let group = deployment_data("open-webui-1");
        let removed = data_root.join("deployment/open-webui-1/web-data");
        let kept = [
            data_root.join("deployment/open-webui-2/web-data"),
            data_root.join("application/open-webui-1/web-data"),
        ];
        for folder in kept.iter().chain([&removed]) {
            std::fs::create_dir_all(folder).expect("data folder");
        }

        assert!(backend.storage_exists(&group).await.expect("probe"));
        backend.remove_storage(&group).await.expect("data removal");

        assert!(!data_root.join("deployment/open-webui-1").exists());
        assert!(!backend.storage_exists(&group).await.expect("probe"));
        for folder in kept {
            assert!(folder.is_dir(), "{}", folder.display());
        }
    }

    #[tokio::test]
    #[ignore = "hardware: just backend::test-docker"]
    async fn hw_docker_storage_removal_deletes_root_owned_content_and_tolerates_absence() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("data root");
        let data_root = directory.path().join("applications-data");
        let backend = DockerRuntimeBackend::connect(DockerRuntimeConfig {
            application_data_root: data_root.clone(),
            ..config()
        })
        .await
        .expect("Docker runtime");
        let group = deployment_data("worker-app-1");
        let storage = data_root.join("deployment/worker-app-1/web-data");
        std::fs::create_dir_all(&storage).expect("storage directory");
        let written = std::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "--volume",
                &format!("{}:/data", storage.display()),
                super::super::SITE_DATA_CLEANUP_IMAGE,
                "sh",
                "-c",
                "mkdir /data/private && echo secret > /data/private/file && chmod 700 /data/private",
            ])
            .status()
            .expect("docker run");
        assert!(written.success(), "root-owned content written");
        // Docker Desktop's file sharing maps container-root files to the host user, so only
        // native Linux bind mounts leave content this process cannot delete; elsewhere the
        // attempt would remove the data itself.
        if cfg!(target_os = "linux") {
            assert!(
                std::fs::remove_dir_all(data_root.join("deployment/worker-app-1")).is_err(),
                "this process cannot remove root-owned content itself"
            );
        }

        assert!(backend.storage_exists(&group).await.expect("probe"));
        backend
            .remove_storage(&group)
            .await
            .expect("root-owned data removal");
        assert!(!backend.storage_exists(&group).await.expect("probe"));
        backend
            .remove_storage(&group)
            .await
            .expect("absent data is already removed");
    }

    fn deployment_data(key: &str) -> WorkloadStorageKey {
        WorkloadStorageKey {
            kind: dotcore_runtime::WorkloadStorageKind::Data,
            namespace: "deployment".to_owned(),
            key: key.to_owned(),
        }
    }

    fn config() -> DockerRuntimeConfig {
        DockerRuntimeConfig {
            site_id: "site-one".to_owned(),
            execution_target_id: "test-target".to_owned(),
            approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
            application_data_root: dotcore_domain::test_support::canonical_temp_dir(),
            workload_cache_root: dotcore_domain::test_support::canonical_temp_dir(),
            accelerator_transport: DockerAcceleratorTransport::LegacyNvidia,
            source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
        }
    }

    fn workload() -> WorkloadLaunch {
        WorkloadLaunch {
            correlation: WorkloadCorrelation { owner_id: "deployment-one".to_owned(), reconcile_generation: 1 },
            execution_target: dotcore_runtime::ExecutionTarget { managed_node_id: SiteNodeId::for_test("node-one"), accelerator_device_ids: vec!["0".to_owned()] },
            spec: WorkloadSpec {
                executable: WorkloadExecutable { artifact: WorkloadExecutableArtifact::OciImage("example.test/workload@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()), path: "/bin/server".to_owned() },
                required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
                arguments: vec![WorkloadValue::literal("--serve")],
                environment: BTreeMap::from([("EXPLICIT_VALUE".to_owned(), WorkloadValue::literal("value"))]),
                artifacts: vec![WorkloadArtifactBinding { name: "data".to_owned(), source: dotcore_domain::test_support::canonical_temp_dir(), destination: "/var/lib/workload".to_owned(), read_only: true }],
                storage: Vec::new(),
                ports: vec![WorkloadPort { name: "http".to_owned(), container_port: 8080 }],
                accelerator_device_ids: vec!["0".to_owned()],
                required_accelerator_device_count: 1,
                ipc_mode: dotcore_runtime::WorkloadIpcMode::Isolated,
                expected_memory: Default::default(),
            },
        }
    }

    #[tokio::test]
    async fn reserved_development_namespace_rejects_existing_site_owned_evidence() {
        let mutations = Arc::new(Mutex::new(0));
        let adapter = DockerAdapter::new(
            TestDockerClient::with_discovered(mutations.clone(), vec![owned_container()]),
            config(),
        );

        let error = adapter
            .ensure_development_namespace_empty()
            .await
            .expect_err("existing site-owned namespace evidence");

        assert_eq!(
            error.to_string(),
            "Invalid runtime launch: Docker runtime rejected existing ownership evidence in the reserved development namespace"
        );
        assert_eq!(*mutations.lock().expect("mutations"), 0);
    }

    #[tokio::test]
    async fn rejects_transport_identifier_mismatch_before_docker_mutation() {
        for (transport, device_id) in [
            (DockerAcceleratorTransport::Cdi, "0"),
            (DockerAcceleratorTransport::LegacyNvidia, "nvidia.com/gpu=0"),
        ] {
            let mutations = Arc::new(Mutex::new(0));
            let mut config = config();
            config.accelerator_transport = transport;
            let mut workload = workload();
            workload.execution_target.accelerator_device_ids = vec![device_id.to_owned()];
            workload.spec.accelerator_device_ids = vec![device_id.to_owned()];
            let adapter = DockerAdapter::new(TestDockerClient::new(mutations.clone()), config);

            assert!(adapter.create_workload(&workload).await.is_err());
            assert_eq!(*mutations.lock().expect("mutations"), 0);
        }
    }

    #[tokio::test]
    async fn rejects_malformed_transport_identifiers_before_docker_mutation() {
        for (transport, device_id) in [
            (DockerAcceleratorTransport::Cdi, "nvidia.com/gpu"),
            (DockerAcceleratorTransport::Cdi, "gpu=0"),
            (DockerAcceleratorTransport::LegacyNvidia, "-1"),
            (DockerAcceleratorTransport::LegacyNvidia, "gpu0"),
        ] {
            let mutations = Arc::new(Mutex::new(0));
            let mut config = config();
            config.accelerator_transport = transport;
            let mut workload = workload();
            workload.execution_target.accelerator_device_ids = vec![device_id.to_owned()];
            workload.spec.accelerator_device_ids = vec![device_id.to_owned()];
            let adapter = DockerAdapter::new(TestDockerClient::new(mutations.clone()), config);

            assert!(adapter.create_workload(&workload).await.is_err());
            assert_eq!(*mutations.lock().expect("mutations"), 0);
        }
    }

    #[tokio::test]
    async fn accepts_a_tagged_image_at_the_docker_adapter_boundary() {
        let mutations = Arc::new(Mutex::new(0));
        let original = ClientError::InvalidResponse(anyhow!("stop after create boundary"));
        let adapter = DockerAdapter::new(
            TestDockerClient::create_failure(mutations.clone(), original),
            config(),
        );
        let mut workload = workload();
        workload.spec.executable.artifact =
            WorkloadExecutableArtifact::OciImage("example.test/workload:dev".to_owned());

        let error = adapter
            .create_workload(&workload)
            .await
            .expect_err("test client stops at Docker create");

        assert!(matches!(error, RuntimeBackendError::Docker(_)));
        assert_eq!(*mutations.lock().expect("mutations"), 1);
    }

    #[tokio::test]
    async fn validates_launch_before_docker_mutation() {
        let mutations = Arc::new(Mutex::new(0));
        let adapter = DockerAdapter::new(TestDockerClient::new(mutations.clone()), config());
        let mut workload = workload();
        workload.spec.executable.artifact =
            WorkloadExecutableArtifact::OciImage("Example.test/workload:dev".to_owned());

        assert!(adapter.create_workload(&workload).await.is_err());
        assert_eq!(*mutations.lock().expect("mutations"), 0);
    }

    #[tokio::test]
    async fn create_error_keeps_workload_context_and_original_docker_error() {
        let mutations = Arc::new(Mutex::new(0));
        let original = ClientError::InvalidResponse(anyhow!("daemon rejected the image"));
        let adapter = DockerAdapter::new(
            TestDockerClient::create_failure(mutations.clone(), original),
            config(),
        );

        let error = adapter
            .create_workload(&workload())
            .await
            .expect_err("Docker creation should fail");

        assert_eq!(
            error.to_string(),
            "Docker runtime failure: Docker container creation failed for workload dotcore_site-one_deployment-one_1"
        );
        let RuntimeBackendError::Docker(docker_error) = &error else {
            unreachable!("Docker runtime error");
        };
        assert_eq!(
            docker_error.to_string(),
            "Docker container creation failed for workload dotcore_site-one_deployment-one_1"
        );
        let contextual = Error::source(&error).expect("contextual anyhow error");
        assert_eq!(
            contextual.to_string(),
            "Docker container creation failed for workload dotcore_site-one_deployment-one_1"
        );
        assert_eq!(
            contextual
                .source()
                .expect("original Docker client error")
                .to_string(),
            "Invalid Docker Engine response: daemon rejected the image"
        );
        assert!(
            docker_error
                .chain()
                .any(|source| source.is::<ClientError>())
        );
        assert_eq!(*mutations.lock().expect("mutations"), 1);
    }

    #[tokio::test]
    async fn discovery_ignores_foreign_malformed_runtime_containers() {
        let mutations = Arc::new(Mutex::new(0));
        let mut foreign = owned_container();
        foreign.id.clear();
        foreign.name.clear();
        foreign.name_is_unambiguous = false;
        foreign
            .labels
            .insert(LABEL_SITE.to_owned(), "other-site".to_owned());
        foreign.labels.remove(LABEL_SCHEMA);

        let adapter = DockerAdapter::new(
            TestDockerClient::with_discovered(mutations, vec![owned_container(), foreign]),
            config(),
        );

        let observations = adapter
            .discover_workloads()
            .await
            .expect("foreign managed containers are outside this site");
        assert_eq!(observations.len(), 1);
        assert!(matches!(
            &observations[0],
            WorkloadObservation::Running(observed)
                if observed.workload_id.0 == "dotcore_site-one_deployment-one_1"
        ));
    }

    #[test]
    fn correlation_lookup_requires_one_full_match_without_exposing_private_identity() {
        let adapter =
            DockerAdapter::new(TestDockerClient::unowned(Arc::new(Mutex::new(0))), config());
        for (workload_id, generation) in [("private-first", 1), ("private-second", 2)] {
            let workload_id = WorkloadId(workload_id.to_owned());
            let mut identity = observed(&workload_id);
            identity.correlation.reconcile_generation = generation;
            adapter.known.lock().expect("registry").insert(
                workload_id.0.clone(),
                KnownWorkload {
                    container_id: format!("container-{generation}"),
                    identity,
                    ports: Vec::new(),
                    launch_fingerprint: "a".repeat(64),
                    managed_node_id: None,
                },
            );
        }

        assert_eq!(
            adapter
                .workload_id_for_correlation(&WorkloadCorrelation {
                    owner_id: "deployment-one".to_owned(),
                    reconcile_generation: 2,
                })
                .expect("full correlation"),
            WorkloadId("private-second".to_owned())
        );
        let error = adapter
            .workload_id_for_correlation(&WorkloadCorrelation {
                owner_id: "private-owner-value".to_owned(),
                reconcile_generation: 2,
            })
            .expect_err("owner mismatch");
        assert!(matches!(error, RuntimeBackendError::WorkloadNotCreated));
        assert_eq!(
            error.to_string(),
            "Runtime workload has not been created yet"
        );
        assert!(!error.to_string().contains("private-owner-value"));
        assert!(!error.to_string().contains("private-second"));

        let duplicate = WorkloadId("private-duplicate".to_owned());
        let mut identity = observed(&duplicate);
        identity.correlation.reconcile_generation = 2;
        adapter.known.lock().expect("registry").insert(
            duplicate.0.clone(),
            KnownWorkload {
                container_id: "container-duplicate".to_owned(),
                identity,
                ports: Vec::new(),
                launch_fingerprint: "a".repeat(64),
                managed_node_id: None,
            },
        );
        let ambiguous = adapter
            .workload_id_for_correlation(&WorkloadCorrelation {
                owner_id: "deployment-one".to_owned(),
                reconcile_generation: 2,
            })
            .expect_err("ambiguous correlation");
        assert!(matches!(ambiguous, RuntimeBackendError::InvalidLaunch(_)));
    }

    #[tokio::test]
    async fn unowned_same_name_replacement_is_never_reported_or_mutated() {
        let mutations = Arc::new(Mutex::new(0));
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        let adapter = DockerAdapter::new(TestDockerClient::unowned(mutations.clone()), config());
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: None,
            },
        );

        assert!(adapter.inspect_workload(&workload_id).await.is_err());
        assert!(adapter.stop_workload(&workload_id).await.is_err());
        assert!(adapter.remove_workload(&workload_id).await.is_err());
        assert_eq!(*mutations.lock().expect("mutations"), 0);
    }

    #[tokio::test]
    async fn reads_exact_owned_docker_source_with_public_descriptor_and_bounded_history() {
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_704_067_200);
        let mut oversized = vec![b'x'; super::super::logs::MAX_LOG_LINE_BYTES + 7];
        oversized.extend_from_slice(b"\nlast\n");
        let adapter = DockerAdapter::new(
            TestDockerClient::with_logs(vec![
                Ok(DockerLogOutput::stdout(timestamp, b"first\npar".to_vec())),
                Ok(DockerLogOutput::stdout(timestamp, b"tial\n".to_vec())),
                Ok(DockerLogOutput::stderr(timestamp, oversized)),
            ]),
            config(),
        );
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: None,
            },
        );
        let source = RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        };
        let query = RuntimeWorkloadLogQuery::new(
            source.clone(),
            RuntimeLogQuery::new(std::num::NonZeroU16::new(4).expect("non-zero"), None)
                .expect("bounded query"),
        );

        let history = adapter
            .read_workload_logs(&workload_id, &query)
            .await
            .expect("owned source history");

        assert_eq!(history.entries.len(), 4);
        assert!(history.entries.iter().all(|entry| entry.source == source));
        assert_eq!(history.entries[0].content, b"first");
        assert_eq!(history.entries[1].content, b"partial");
        assert_eq!(
            history.entries[2].stream,
            dotcore_runtime::RuntimeLogStreamKind::Stderr
        );
        assert_eq!(
            history.entries[2].content,
            vec![b'x'; super::super::logs::MAX_LOG_LINE_BYTES]
        );
        assert!(history.entries[2].truncated);
        assert_eq!(history.entries[3].content, b"last");
    }

    #[tokio::test]
    async fn rejects_cancelled_follow_before_opening_an_owned_docker_source() {
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        let adapter = DockerAdapter::new(TestDockerClient::new(Arc::new(Mutex::new(0))), config());
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: None,
            },
        );
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let query = dotcore_runtime::RuntimeWorkloadLogFollowQuery::new(
            RuntimeLogSource {
                name: "model-primary".to_owned(),
                label: "Primary model".to_owned(),
            },
            dotcore_runtime::RuntimeLogFollowQuery::new(None, cancellation),
        );

        let error = match adapter.follow_workload_logs(&workload_id, query).await {
            Ok(_) => panic!("cancelled follow must not open Docker logs"),
            Err(error) => error,
        };

        assert!(error.is_log_cancelled());
        assert_eq!(
            error.to_string(),
            "Runtime direct diagnostics request was cancelled"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn discovery_restores_managed_node_process_identity() {
        let client =
            TestDockerClient::with_discovered(Arc::new(Mutex::new(0)), vec![owned_container()]);
        let adapter =
            DockerAdapter::new_with_node(client, config(), Some(SiteNodeId::for_test("node-test")));

        let observations = adapter
            .discover_workloads()
            .await
            .expect("owned workload discovery");

        let WorkloadObservation::Running(workload) = &observations[0] else {
            panic!("owned running workload must be observed as running");
        };
        assert_eq!(
            workload.host_process,
            Some(dotcore_domain::host_process::HostProcessIdentity {
                managed_node_id: SiteNodeId::for_test("node-test"),
                process_id: std::process::id(),
                process_start_token: dotcore_host_process::read_process_start_token(
                    std::process::id(),
                )
                .expect("current process start token"),
            })
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_discovery_reports_no_host_process_for_a_running_host_pid() {
        let client =
            TestDockerClient::with_discovered(Arc::new(Mutex::new(0)), vec![owned_container()]);
        let adapter =
            DockerAdapter::new_with_node(client, config(), Some(SiteNodeId::for_test("node-test")));

        let observations = adapter
            .discover_workloads()
            .await
            .expect("owned workload discovery");

        let WorkloadObservation::Running(workload) = &observations[0] else {
            panic!("owned running workload must be observed as running");
        };
        // The fixture container PID is this test process, which runs on the Mac host.
        assert!(dotcore_host_process::read_process_start_token(std::process::id()).is_ok());
        assert_eq!(workload.host_process, None);
    }

    #[tokio::test]
    async fn known_workload_recovery_keeps_the_typed_node_process_identity() {
        let mutations = Arc::new(Mutex::new(0));
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        let adapter = DockerAdapter::new_with_node(
            TestDockerClient::new(mutations.clone()),
            config(),
            Some(SiteNodeId::for_test("node-test")),
        );
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: Some(SiteNodeId::for_test("node-test")),
            },
        );

        let observation = adapter
            .create_workload(&workload())
            .await
            .expect("known workload recovery");

        let WorkloadObservation::Running(recovered) = observation else {
            panic!("recovered owned workload must be observed as running");
        };
        assert_eq!(recovered.workload_id, workload_id);
        assert_eq!(recovered.host_process, expected_host_process("node-test"));
        assert_eq!(*mutations.lock().expect("mutations"), 0);
    }

    #[tokio::test]
    async fn inspection_reports_declared_loopback_endpoints_and_private_process_facts() {
        let adapter = DockerAdapter::new(TestDockerClient::new(Arc::new(Mutex::new(0))), config());
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: Some(SiteNodeId::for_test("node-test")),
            },
        );

        let endpoints = vec![LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: 50000,
        }];
        assert_eq!(
            adapter
                .inspect_workload(&workload_id)
                .await
                .expect("loopback endpoint observation"),
            WorkloadObservation::Running(ObservedWorkload {
                loopback_endpoints: endpoints.clone(),
                services: vec![WorkloadServiceBinding::loopback(
                    workload_id.0.clone(),
                    "http",
                    50000,
                    endpoints[0].clone(),
                )],
                host_process: expected_host_process("node-test"),
                ..observed(&workload_id)
            })
        );
        assert_eq!(
            adapter
                .inspect_development_process(&workload_id)
                .await
                .expect("private process facts"),
            DockerDevelopmentProcess {
                container_id: "owned".to_owned(),
                process_id: Some(std::process::id()),
                launch_fingerprint: "a".repeat(64),
                loopback_endpoints: endpoints,
            }
        );
    }

    #[tokio::test]
    async fn retained_workload_restart_rebinds_a_reassigned_loopback_port() {
        let client = TestDockerClient::new(Arc::new(Mutex::new(0)));
        let host_port = client.host_port.clone();
        let (adapter, workload_id) = adapter_with_known_workload(client);

        let WorkloadObservation::Running(initial) = adapter
            .inspect_workload(&workload_id)
            .await
            .expect("initial workload inspection")
        else {
            panic!("initial workload must be running");
        };
        assert_eq!(
            initial.services,
            vec![WorkloadServiceBinding::loopback(
                workload_id.0.clone(),
                "http",
                50000,
                LoopbackEndpoint {
                    port_name: "http".to_owned(),
                    host_port: 50000,
                },
            )]
        );

        adapter
            .stop_workload(&workload_id)
            .await
            .expect("retained workload stop without remapping");
        let WorkloadObservation::Running(retained) = adapter
            .start_workload(&workload_id)
            .await
            .expect("retained workload restart without remapping")
        else {
            panic!("restarted workload must be running");
        };
        assert_eq!(retained.services, initial.services);

        adapter
            .stop_workload(&workload_id)
            .await
            .expect("retained workload stop");
        *host_port.lock().expect("host port") = 50001;
        let WorkloadObservation::Running(resumed) = adapter
            .start_workload(&workload_id)
            .await
            .expect("retained workload restart")
        else {
            panic!("restarted workload must be running");
        };
        assert_eq!(resumed.workload_id, initial.workload_id);
        assert_eq!(
            resumed.services,
            vec![WorkloadServiceBinding::loopback(
                workload_id.0.clone(),
                "http",
                50001,
                LoopbackEndpoint {
                    port_name: "http".to_owned(),
                    host_port: 50001,
                },
            )]
        );

        let WorkloadObservation::Running(unchanged) = adapter
            .inspect_workload(&workload_id)
            .await
            .expect("unchanged resumed workload inspection")
        else {
            panic!("resumed workload must remain running");
        };
        assert_eq!(unchanged.services, resumed.services);
    }

    #[tokio::test]
    async fn retained_workload_start_recreates_its_deleted_cache_folder() {
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-adapter-workload-cache-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("cache root");
        let root = root.canonicalize().expect("canonical cache root");
        let cache = root.join("site-one/deployment/deployment-one/engine-cache");
        let mut client = TestDockerClient::new(Arc::new(Mutex::new(0)));
        client.bind_sources = vec![
            dotcore_domain::test_support::canonical_temp_dir(),
            cache.clone(),
        ];
        let (mut adapter, workload_id) = adapter_with_known_workload(client);
        adapter.config.workload_cache_root = root.clone();
        assert!(!cache.exists());

        adapter
            .start_workload(&workload_id)
            .await
            .expect("retained workload start");

        assert!(cache.is_dir());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn inspection_rejects_a_zero_loopback_host_port() {
        let client = TestDockerClient::new(Arc::new(Mutex::new(0)));
        *client.host_port.lock().expect("host port") = 0;
        let (adapter, workload_id) = adapter_with_known_workload(client);

        let error = adapter
            .inspect_workload(&workload_id)
            .await
            .expect_err("zero host port must not produce a service binding");

        assert_eq!(
            error.to_string(),
            "Invalid runtime launch: Docker workload owned has invalid zero loopback host port for declared port http"
        );
    }

    fn adapter_with_known_workload(
        client: TestDockerClient,
    ) -> (DockerAdapter<TestDockerClient>, WorkloadId) {
        let adapter = DockerAdapter::new(client, config());
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: None,
            },
        );

        (adapter, workload_id)
    }

    #[tokio::test]
    async fn stopped_inspection_exposes_terminal_exit_status() {
        let mut client = TestDockerClient::new(Arc::new(Mutex::new(0)));
        client.stopped_exit_status = Some(17);
        let adapter = DockerAdapter::new(client, config());
        let workload_id = WorkloadId("dotcore_site-one_deployment-one_1".to_owned());
        adapter.known.lock().expect("registry").insert(
            workload_id.0.clone(),
            KnownWorkload {
                container_id: "owned".to_owned(),
                identity: observed(&workload_id),
                ports: vec![DockerPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                launch_fingerprint: "a".repeat(64),
                managed_node_id: None,
            },
        );
        let observation = adapter
            .inspect_workload(&workload_id)
            .await
            .expect("inspection");
        let WorkloadObservation::Stopped {
            workload,
            exit_status,
        } = observation
        else {
            panic!("expected a stopped workload");
        };
        assert_eq!(exit_status, Some(17));
        assert_eq!(workload.workload_id, workload_id);
    }

    /// The test process identity the adapter reports for a node. Only Linux resolves it; on macOS
    /// container PIDs belong to Docker's Linux virtual machine, so no host process is reported.
    #[cfg(target_os = "linux")]
    fn expected_host_process(
        node_id: &str,
    ) -> Option<dotcore_domain::host_process::HostProcessIdentity> {
        Some(dotcore_domain::host_process::HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test(node_id),
            process_id: std::process::id(),
            process_start_token: dotcore_host_process::read_process_start_token(std::process::id())
                .expect("current process start token"),
        })
    }

    #[cfg(target_os = "macos")]
    fn expected_host_process(
        _node_id: &str,
    ) -> Option<dotcore_domain::host_process::HostProcessIdentity> {
        None
    }

    fn observed(workload_id: &WorkloadId) -> ObservedWorkload {
        ObservedWorkload {
            workload_id: workload_id.clone(),
            correlation: dotcore_runtime::WorkloadCorrelation {
                owner_id: "deployment-one".to_owned(),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId(BACKEND_ID.to_owned()),
            loopback_endpoints: Vec::new(),
            services: vec![WorkloadServiceBinding::loopback(
                workload_id.0.clone(),
                "http",
                1,
                LoopbackEndpoint {
                    port_name: "http".to_owned(),
                    host_port: 50000,
                },
            )],
            host_process: None,
        }
    }

    struct TestDockerClient {
        mutations: Arc<Mutex<usize>>,
        live: DockerContainer,
        discovered: Vec<DockerContainer>,
        create_error: Arc<Mutex<Option<ClientError>>>,
        logs: Arc<Mutex<Vec<Result<DockerLogOutput, ClientError>>>>,
        host_port: Arc<Mutex<u16>>,
        stopped_exit_status: Option<i64>,
        bind_sources: Vec<std::path::PathBuf>,
    }

    impl TestDockerClient {
        fn new(mutations: Arc<Mutex<usize>>) -> Self {
            Self {
                mutations,
                live: owned_container(),
                discovered: Vec::new(),
                create_error: Arc::new(Mutex::new(None)),
                logs: Arc::new(Mutex::new(Vec::new())),
                host_port: Arc::new(Mutex::new(50000)),
                stopped_exit_status: None,
                bind_sources: Vec::new(),
            }
        }

        fn with_logs(logs: Vec<Result<DockerLogOutput, ClientError>>) -> Self {
            Self {
                mutations: Arc::new(Mutex::new(0)),
                live: owned_container(),
                discovered: Vec::new(),
                create_error: Arc::new(Mutex::new(None)),
                logs: Arc::new(Mutex::new(logs)),
                host_port: Arc::new(Mutex::new(50000)),
                stopped_exit_status: None,
                bind_sources: Vec::new(),
            }
        }

        fn with_discovered(mutations: Arc<Mutex<usize>>, discovered: Vec<DockerContainer>) -> Self {
            Self {
                mutations,
                live: owned_container(),
                discovered,
                create_error: Arc::new(Mutex::new(None)),
                logs: Arc::new(Mutex::new(Vec::new())),
                host_port: Arc::new(Mutex::new(50000)),
                stopped_exit_status: None,
                bind_sources: Vec::new(),
            }
        }

        fn create_failure(mutations: Arc<Mutex<usize>>, error: ClientError) -> Self {
            Self {
                mutations,
                live: owned_container(),
                discovered: Vec::new(),
                create_error: Arc::new(Mutex::new(Some(error))),
                logs: Arc::new(Mutex::new(Vec::new())),
                host_port: Arc::new(Mutex::new(50000)),
                stopped_exit_status: None,
                bind_sources: Vec::new(),
            }
        }

        fn unowned(mutations: Arc<Mutex<usize>>) -> Self {
            Self {
                mutations,
                live: DockerContainer {
                    id: "replacement".to_owned(),
                    name: "dotcore_site-one_deployment-one_1".to_owned(),
                    name_is_unambiguous: true,
                    labels: BTreeMap::new(),
                },
                discovered: Vec::new(),
                create_error: Arc::new(Mutex::new(None)),
                logs: Arc::new(Mutex::new(Vec::new())),
                host_port: Arc::new(Mutex::new(50000)),
                stopped_exit_status: None,
                bind_sources: Vec::new(),
            }
        }
    }

    impl DockerClient for TestDockerClient {
        fn list_managed_containers(
            &self,
        ) -> impl Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send {
            ready(Ok(self.discovered.clone()))
        }

        fn list_site_containers(
            &self,
            site_id: &str,
        ) -> impl Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send {
            ready(Ok(self
                .discovered
                .iter()
                .filter(|container| {
                    container.labels.get(LABEL_SITE).map(String::as_str) == Some(site_id)
                })
                .cloned()
                .collect()))
        }

        fn create_container(
            &self,
            _request: &DockerCreateRequest,
        ) -> impl Future<Output = Result<String, ClientError>> + Send {
            *self.mutations.lock().expect("mutations") += 1;
            if let Some(error) = self.create_error.lock().expect("create error").take() {
                return ready(Err(error));
            }
            ready(Ok("created".to_owned()))
        }
        fn container_logs(&self, _request: DockerLogRequest) -> DockerLogStream {
            Box::pin(futures_util::stream::iter(std::mem::take(
                &mut *self.logs.lock().expect("logs"),
            )))
        }
        fn inspect_owned_container(
            &self,
            _container_id: &str,
        ) -> impl Future<Output = Result<DockerContainer, ClientError>> + Send {
            ready(Ok(self.live.clone()))
        }
        fn container_exists(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<bool, ClientError>> + Send {
            ready(Ok(false))
        }
        fn image_exists(
            &self,
            _image: &str,
        ) -> impl Future<Output = Result<bool, ClientError>> + Send {
            ready(Ok(true))
        }
        fn start_container(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            *self.mutations.lock().expect("mutations") += 1;
            ready(Ok(()))
        }
        fn inspect_container(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<ContainerInspection, ClientError>> + Send {
            ready(Ok(ContainerInspection {
                running: self.stopped_exit_status.is_none(),
                exit_status: self.stopped_exit_status,
                process_id: self.stopped_exit_status.is_none().then(std::process::id),
                host_ports: BTreeMap::from([(8080, *self.host_port.lock().expect("host port"))]),
                bind_sources: self.bind_sources.clone(),
            }))
        }
        fn stop_container(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            *self.mutations.lock().expect("mutations") += 1;
            ready(Ok(()))
        }
        fn remove_container(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            *self.mutations.lock().expect("mutations") += 1;
            ready(Ok(()))
        }
    }

    fn owned_container() -> DockerContainer {
        DockerContainer {
            id: "owned".to_owned(),
            name: "dotcore_site-one_deployment-one_1".to_owned(),
            name_is_unambiguous: true,
            labels: BTreeMap::from([
                (LABEL_MANAGED.to_owned(), "true".to_owned()),
                (LABEL_SITE.to_owned(), "site-one".to_owned()),
                (LABEL_EXECUTION_TARGET.to_owned(), "test-target".to_owned()),
                (LABEL_SCHEMA.to_owned(), OWNERSHIP_SCHEMA.to_owned()),
                (LABEL_CORRELATION.to_owned(), "deployment-one".to_owned()),
                (
                    LABEL_WORKLOAD.to_owned(),
                    "dotcore_site-one_deployment-one_1".to_owned(),
                ),
                (LABEL_GENERATION.to_owned(), "1".to_owned()),
                (LABEL_PORTS.to_owned(), "http=8080".to_owned()),
                (LABEL_LAUNCH_FINGERPRINT.to_owned(), "a".repeat(64)),
            ]),
        }
    }
}
