//! Private boundary over the local Docker Engine API: it opens the daemon connection
//! and performs each container and network operation the Docker backend needs (list,
//! create, inspect, start, stop, remove, logs, stats, networks).
//!
//! The operation set is an injectable contract so tests can supply a fake, every
//! operation is bounded by a thirty-second timeout, and every failure surfaces as a
//! typed error.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::Future,
    pin::Pin,
    time::SystemTime,
};

use anyhow::anyhow;
use bollard::{
    Docker,
    container::LogOutput,
    models::{
        ContainerCreateBody, ContainerSummary, DeviceRequest, EndpointSettings, HostConfig,
        HostConfigLogConfig, Mount, MountType, NetworkCreateRequest, NetworkingConfig, PortBinding,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, InspectNetworkOptionsBuilder, ListContainersOptionsBuilder,
        ListNetworksOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
        StatsOptionsBuilder, StopContainerOptionsBuilder,
    },
};
use dotcore_domain::docker::DOCKER_OPERATION_TIMEOUT;
use dotcore_domain::real_time;
use futures_util::{Stream, StreamExt as _, stream};

use super::{
    ownership::{LABEL_MANAGED, LABEL_SITE},
    request::{
        DockerCreateRequest, DockerDeviceRequest, DockerGroupContainerRequest,
        DockerGroupNetworkRequest, DockerMount,
    },
};

/// Log driver of every workload container, so log history does not depend on the daemon default
/// (a journald default drops lines under its rate limit) and keeps its rotated size bound.
pub(super) const WORKLOAD_LOG_DRIVER: &str = "local";
const CONTAINER_EXISTENCE_INSPECTION: &str = "Docker container existence inspection";
const DOCKER_DESKTOP_ANNOTATION_PREFIX: &str = "desktop.docker.io/";
const LOCAL_IMAGE_AVAILABILITY_INSPECTION: &str = "Docker local image availability inspection";

/// Private Docker Engine client boundary used by the adapter and its unit tests.
pub(super) trait DockerClient: Send + Sync {
    fn list_managed_containers(
        &self,
    ) -> impl Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send;
    fn list_site_containers(
        &self,
        _site_id: &str,
    ) -> impl Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send {
        async { Err(ClientError::new("Docker site containers are unavailable")) }
    }
    fn create_container(
        &self,
        request: &DockerCreateRequest,
    ) -> impl Future<Output = Result<String, ClientError>> + Send;

    fn inspect_owned_container(
        &self,
        container_id: &str,
    ) -> impl Future<Output = Result<DockerContainer, ClientError>> + Send;

    fn container_exists(
        &self,
        workload_id: &str,
    ) -> impl Future<Output = Result<bool, ClientError>> + Send;

    fn image_exists(&self, image: &str) -> impl Future<Output = Result<bool, ClientError>> + Send;

    fn execution_platform(
        &self,
    ) -> impl Future<Output = Result<dotcore_domain::ExecutionPlatform, ClientError>> + Send {
        async {
            Err(ClientError::new(
                "Docker destination platform is unavailable",
            ))
        }
    }

    fn inspect_image_platform(
        &self,
        _image: &str,
    ) -> impl Future<Output = Result<dotcore_domain::ExecutionPlatform, ClientError>> + Send {
        async { Err(ClientError::new("Docker image platform is unavailable")) }
    }

    fn start_container(
        &self,
        workload_id: &str,
    ) -> impl Future<Output = Result<(), ClientError>> + Send;

    fn inspect_container(
        &self,
        workload_id: &str,
    ) -> impl Future<Output = Result<ContainerInspection, ClientError>> + Send;

    fn stop_container(
        &self,
        workload_id: &str,
    ) -> impl Future<Output = Result<(), ClientError>> + Send;

    fn remove_container(
        &self,
        workload_id: &str,
    ) -> impl Future<Output = Result<(), ClientError>> + Send;

    fn container_logs(&self, _request: DockerLogRequest) -> DockerLogStream {
        Box::pin(futures_util::stream::once(async {
            Err(ClientError::new("Docker container logs are unavailable"))
        }))
    }

    fn inspect_owned_network(
        &self,
        _network_id: &str,
    ) -> impl Future<Output = Result<DockerNetwork, ClientError>> + Send {
        async { Err(ClientError::new("Docker group networks are unavailable")) }
    }
    fn list_group_networks(
        &self,
        _network_name: &str,
    ) -> impl Future<Output = Result<Vec<DockerNetwork>, ClientError>> + Send {
        async { Err(ClientError::new("Docker group networks are unavailable")) }
    }
    fn list_managed_group_network_summaries(
        &self,
    ) -> impl Future<Output = Result<Vec<DockerNetworkSummary>, ClientError>> + Send {
        async { Err(ClientError::new("Docker group networks are unavailable")) }
    }
    fn list_site_group_network_summaries(
        &self,
        _site_id: &str,
    ) -> impl Future<Output = Result<Vec<DockerNetworkSummary>, ClientError>> + Send {
        async {
            Err(ClientError::new(
                "Docker site group networks are unavailable",
            ))
        }
    }
    fn list_managed_group_networks(
        &self,
    ) -> impl Future<Output = Result<Vec<DockerNetwork>, ClientError>> + Send {
        async { Err(ClientError::new("Docker group networks are unavailable")) }
    }
    fn create_group_network(
        &self,
        _request: &DockerGroupNetworkRequest,
    ) -> impl Future<Output = Result<String, ClientError>> + Send {
        async { Err(ClientError::new("Docker group networks are unavailable")) }
    }
    fn remove_network(
        &self,
        _network_id: &str,
    ) -> impl Future<Output = Result<(), ClientError>> + Send {
        async { Err(ClientError::new("Docker group networks are unavailable")) }
    }
    fn create_group_container(
        &self,
        _request: &DockerGroupContainerRequest,
    ) -> impl Future<Output = Result<String, ClientError>> + Send {
        async { Err(ClientError::new("Docker group containers are unavailable")) }
    }
    fn inspect_group_container(
        &self,
        _container_id: &str,
    ) -> impl Future<Output = Result<GroupContainerInspection, ClientError>> + Send {
        async { Err(ClientError::new("Docker group containers are unavailable")) }
    }
    fn inspect_group_container_ownership(
        &self,
        _container_id: &str,
    ) -> impl Future<Output = Result<GroupContainerOwnership, ClientError>> + Send {
        async { Err(ClientError::new("Docker group containers are unavailable")) }
    }
    fn inspect_group_execution(
        &self,
        _container_id: &str,
    ) -> impl Future<Output = Result<DockerGroupExecution, ClientError>> + Send {
        async { Err(ClientError::new("Docker group containers are unavailable")) }
    }
    fn container_stats(
        &self,
        _container_id: &str,
    ) -> impl Future<Output = Result<DockerContainerStats, ClientError>> + Send {
        async { Err(ClientError::new("Docker container stats are unavailable")) }
    }
}

/// Local Docker Engine client for the configured host daemon.
pub(super) struct LocalDockerClient {
    docker: Docker,
}

impl LocalDockerClient {
    /// Connects to the configured local Docker daemon.
    pub(super) fn connect() -> Result<Self, ClientError> {
        Self::connect_at(&dotcore_domain::docker::local_endpoint())
    }

    /// Connects to the Docker daemon behind one Unix socket. The connection is lazy: only the
    /// socket path must exist until the first call.
    pub(super) fn connect_at(endpoint: &str) -> Result<Self, ClientError> {
        Docker::connect_with_unix(
            endpoint,
            dotcore_domain::docker::DOCKER_CLIENT_TIMEOUT_SECONDS,
            bollard::API_DEFAULT_VERSION,
        )
        .map(|docker| Self { docker })
        .map_err(ClientError::from)
    }

    /// Negotiates the Docker API version with a bounded call.
    pub(super) async fn negotiate_version(self) -> Result<Self, ClientError> {
        let docker = bounded_operation(
            "Docker API version negotiation",
            self.docker.negotiate_version(),
        )
        .await?;
        Ok(Self { docker })
    }

    pub(super) fn raw(&self) -> &Docker {
        &self.docker
    }

    async fn list_containers_by_label(
        &self,
        label: String,
    ) -> Result<Vec<DockerContainer>, ClientError> {
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&HashMap::from([("label", vec![label])]))
            .build();
        bounded_operation(
            "Docker container list",
            self.docker.list_containers(Some(options)),
        )
        .await
        .map(|containers| containers.into_iter().map(listed_container).collect())
    }
}

impl DockerClient for LocalDockerClient {
    async fn list_managed_containers(&self) -> Result<Vec<DockerContainer>, ClientError> {
        self.list_containers_by_label(format!("{LABEL_MANAGED}=true"))
            .await
    }

    async fn list_site_containers(
        &self,
        site_id: &str,
    ) -> Result<Vec<DockerContainer>, ClientError> {
        self.list_containers_by_label(format!("{LABEL_SITE}={site_id}"))
            .await
    }

    async fn create_container(&self, request: &DockerCreateRequest) -> Result<String, ClientError> {
        let body = individual_container_create_body(request);
        let options = CreateContainerOptionsBuilder::default()
            .name(&request.name)
            .build();
        let created = bounded_operation(
            "Docker container create",
            self.docker.create_container(Some(options), body),
        )
        .await?;
        if created.id.is_empty() {
            return Err(ClientError::InvalidResponse(anyhow!(
                "Docker create returned no container ID"
            )));
        }
        Ok(created.id)
    }

    async fn inspect_owned_container(
        &self,
        container_id: &str,
    ) -> Result<DockerContainer, ClientError> {
        let response = bounded_operation(
            "Docker ownership inspect",
            self.docker.inspect_container(container_id, None),
        )
        .await?;
        Ok(DockerContainer {
            id: response.id.unwrap_or_default(),
            name: response
                .name
                .map(|name| name.trim_start_matches('/').to_owned())
                .unwrap_or_default(),
            name_is_unambiguous: true,
            labels: response
                .config
                .and_then(|config| config.labels)
                .unwrap_or_default()
                .into_iter()
                .collect(),
        })
    }

    async fn container_exists(&self, workload_id: &str) -> Result<bool, ClientError> {
        match bounded_operation(
            CONTAINER_EXISTENCE_INSPECTION,
            self.docker.inspect_container(workload_id, None),
        )
        .await
        {
            Ok(_) => Ok(true),
            Err(error) => {
                if error.is_not_found() {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn image_exists(&self, image: &str) -> Result<bool, ClientError> {
        match bounded_operation(
            LOCAL_IMAGE_AVAILABILITY_INSPECTION,
            self.docker.inspect_image(image),
        )
        .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn execution_platform(&self) -> Result<dotcore_domain::ExecutionPlatform, ClientError> {
        let information =
            bounded_operation("Docker destination platform inspection", self.docker.info()).await?;
        super::capabilities::execution_platform_from_information(&information)
            .map_err(|error| ClientError::new(error.to_string()))
    }

    async fn inspect_image_platform(
        &self,
        image: &str,
    ) -> Result<dotcore_domain::ExecutionPlatform, ClientError> {
        let inspection = bounded_operation(
            "Docker group image platform inspection",
            self.docker.inspect_image(image),
        )
        .await?;
        dotcore_domain::ExecutionPlatform::normalize(
            inspection.os.as_deref().unwrap_or_default(),
            inspection.architecture.as_deref().unwrap_or_default(),
        )
        .map_err(|error| ClientError::new(error.to_string()))
    }

    async fn start_container(&self, workload_id: &str) -> Result<(), ClientError> {
        bounded_operation(
            "Docker container start",
            self.docker.start_container(workload_id, None),
        )
        .await
    }

    async fn inspect_container(
        &self,
        workload_id: &str,
    ) -> Result<ContainerInspection, ClientError> {
        let response = bounded_operation(
            "Docker container inspect",
            self.docker.inspect_container(workload_id, None),
        )
        .await?;
        let running = response
            .state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false);
        let process_id = response
            .state
            .as_ref()
            .and_then(|state| state.pid)
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0);
        let exit_status = (!running)
            .then(|| response.state.as_ref().and_then(|state| state.exit_code))
            .flatten();
        let bind_sources = response
            .mounts
            .iter()
            .flatten()
            .filter(|mount| mount.typ.as_deref() == Some("bind"))
            .filter_map(|mount| mount.source.as_deref().map(std::path::PathBuf::from))
            .collect();
        let host_ports = strict_loopback_host_ports(
            response
                .network_settings
                .and_then(|settings| settings.ports)
                .unwrap_or_default(),
        )?;
        Ok(ContainerInspection {
            running,
            process_id,
            exit_status,
            host_ports,
            bind_sources,
        })
    }

    async fn stop_container(&self, workload_id: &str) -> Result<(), ClientError> {
        let options = StopContainerOptionsBuilder::default().t(10).build();
        bounded_operation(
            "Docker container stop",
            self.docker.stop_container(workload_id, Some(options)),
        )
        .await
    }

    async fn remove_container(&self, workload_id: &str) -> Result<(), ClientError> {
        // Anonymous volumes come only from an image's VOLUME declarations, never from a
        // deployment's storage, and a recreated container gets fresh ones; they go with it.
        let options = RemoveContainerOptionsBuilder::default()
            .force(false)
            .v(true)
            .build();
        bounded_operation(
            "Docker container remove",
            self.docker.remove_container(workload_id, Some(options)),
        )
        .await
    }

    fn container_logs(&self, request: DockerLogRequest) -> DockerLogStream {
        let options = LogsOptionsBuilder::default()
            .follow(request.follow)
            .stdout(true)
            .stderr(true)
            .since(request.since.map(docker_timestamp).unwrap_or(0))
            .until(request.until.map(docker_timestamp).unwrap_or(0))
            .timestamps(true)
            .tail(
                &request
                    .tail
                    .map_or_else(|| "all".to_owned(), |tail| tail.to_string()),
            )
            .build();
        let docker = self.docker.clone();
        let logs = docker.logs(&request.container_id, Some(options));
        Box::pin(stream::unfold(
            (logs, DockerLogFramer::default(), VecDeque::new(), false),
            |(mut logs, mut framer, mut pending, mut finished)| async move {
                loop {
                    if let Some(output) = pending.pop_front() {
                        return Some((Ok(output), (logs, framer, pending, finished)));
                    }
                    if finished {
                        return None;
                    }
                    match logs.next().await {
                        Some(Ok(output)) => match framer.push(output) {
                            Ok(outputs) => pending.extend(outputs),
                            Err(error) => return Some((Err(error), (logs, framer, pending, true))),
                        },
                        Some(Err(error)) => {
                            return Some((
                                Err(ClientError::from(error)),
                                (logs, framer, pending, true),
                            ));
                        }
                        None => {
                            pending.extend(framer.finish());
                            finished = true;
                        }
                    }
                }
            },
        ))
    }

    async fn inspect_owned_network(&self, network_id: &str) -> Result<DockerNetwork, ClientError> {
        let network = bounded_operation(
            "Docker group network inspect",
            self.docker
                .inspect_network(network_id, Some(owned_network_inspection_options())),
        )
        .await?;
        Ok(DockerNetwork {
            id: network.id.unwrap_or_default(),
            name: network.name.unwrap_or_default(),
            labels: network.labels.unwrap_or_default().into_iter().collect(),
            internal: network.internal.unwrap_or(false),
            attachable: network.attachable.unwrap_or(false),
            driver: network.driver.unwrap_or_default(),
            attached_container_ids: network.containers.unwrap_or_default().into_keys().collect(),
        })
    }

    async fn list_group_networks(
        &self,
        network_name: &str,
    ) -> Result<Vec<DockerNetwork>, ClientError> {
        let options = ListNetworksOptionsBuilder::default()
            .filters(&HashMap::from([("name", vec![network_name])]))
            .build();
        let networks = bounded_operation(
            "Docker group network list",
            self.docker.list_networks(Some(options)),
        )
        .await?;
        let mut exact = Vec::new();
        for network in networks
            .into_iter()
            .filter(|network| network.name.as_deref() == Some(network_name))
        {
            let network_id = network
                .id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| ClientError::new("Docker group network list returned no ID"))?;
            exact.push(self.inspect_owned_network(&network_id).await?);
        }
        Ok(exact)
    }

    async fn list_managed_group_network_summaries(
        &self,
    ) -> Result<Vec<DockerNetworkSummary>, ClientError> {
        list_network_summaries_by_label(&self.docker, format!("{LABEL_MANAGED}=true")).await
    }

    async fn list_site_group_network_summaries(
        &self,
        site_id: &str,
    ) -> Result<Vec<DockerNetworkSummary>, ClientError> {
        list_network_summaries_by_label(&self.docker, format!("{LABEL_SITE}={site_id}")).await
    }

    async fn list_managed_group_networks(&self) -> Result<Vec<DockerNetwork>, ClientError> {
        let summaries = self.list_managed_group_network_summaries().await?;
        let mut inspected = Vec::new();
        for network in summaries {
            if network.id.is_empty() {
                return Err(ClientError::new(
                    "Docker managed network list returned no ID",
                ));
            }
            inspected.push(self.inspect_owned_network(&network.id).await?);
        }
        Ok(inspected)
    }

    async fn create_group_network(
        &self,
        request: &DockerGroupNetworkRequest,
    ) -> Result<String, ClientError> {
        let created = bounded_operation(
            "Docker group network create",
            self.docker.create_network(NetworkCreateRequest {
                name: request.name.clone(),
                driver: Some("bridge".to_owned()),
                internal: Some(false),
                attachable: Some(false),
                labels: Some(request.labels.clone().into_iter().collect()),
                ..NetworkCreateRequest::default()
            }),
        )
        .await?;
        if created.id.is_empty() {
            return Err(ClientError::new(
                "Docker group network create returned no network ID",
            ));
        }
        Ok(created.id)
    }

    async fn remove_network(&self, network_id: &str) -> Result<(), ClientError> {
        bounded_operation(
            "Docker group network remove",
            self.docker.remove_network(network_id),
        )
        .await
    }

    async fn create_group_container(
        &self,
        request: &DockerGroupContainerRequest,
    ) -> Result<String, ClientError> {
        let body = group_container_create_body(request);
        let options = CreateContainerOptionsBuilder::default()
            .name(&request.name)
            .build();
        let created = bounded_operation(
            "Docker group container create",
            self.docker.create_container(Some(options), body),
        )
        .await?;
        if created.id.is_empty() {
            return Err(ClientError::new(
                "Docker group container create returned no container ID",
            ));
        }
        Ok(created.id)
    }

    async fn inspect_group_container(
        &self,
        container_id: &str,
    ) -> Result<GroupContainerInspection, ClientError> {
        let response = bounded_operation(
            "Docker group container inspect",
            self.docker.inspect_container(container_id, None),
        )
        .await?;
        let state = response.state;
        let running = state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false);
        let process_id = state
            .as_ref()
            .and_then(|state| state.pid)
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0);
        let settings = response.network_settings.unwrap_or_default();
        let private_address = settings.networks.as_ref().and_then(|networks| {
            networks
                .values()
                .find_map(|endpoint| endpoint.ip_address.as_deref()?.parse().ok())
        });
        let networks = settings
            .networks
            .unwrap_or_default()
            .into_iter()
            .map(|(name, endpoint)| {
                (
                    name,
                    GroupNetworkAttachment {
                        network_id: endpoint.network_id,
                        aliases: endpoint.aliases,
                    },
                )
            })
            .collect();
        let host_bindings = settings
            .ports
            .unwrap_or_default()
            .into_iter()
            .map(|(key, bindings)| {
                (
                    key,
                    bindings.map(|bindings| {
                        bindings
                            .into_iter()
                            .map(|binding| GroupPortBinding {
                                host: binding.host_ip,
                                port: binding.host_port,
                            })
                            .collect()
                    }),
                )
            })
            .collect();
        Ok(GroupContainerInspection {
            running,
            process_id,
            host_bindings,
            private_address,
            networks,
        })
    }

    async fn inspect_group_container_ownership(
        &self,
        container_id: &str,
    ) -> Result<GroupContainerOwnership, ClientError> {
        let response = bounded_operation(
            "Docker group ownership inspect",
            self.docker.inspect_container(container_id, None),
        )
        .await?;
        let config = response.config.unwrap_or_default();
        // The image ID stays valid when the creation-time tag is removed or re-pointed.
        let image = response
            .image
            .clone()
            .filter(|image| !image.is_empty())
            .ok_or_else(|| ClientError::new("Docker group container has no image identity"))?;
        let image = bounded_operation(
            "Docker group image inspect",
            self.docker.inspect_image(&image),
        )
        .await?;
        Ok(GroupContainerOwnership {
            id: response.id.unwrap_or_default(),
            name: response
                .name
                .map(|name| name.trim_start_matches('/').to_owned())
                .unwrap_or_default(),
            labels: config.labels.unwrap_or_default().into_iter().collect(),
            image_labels: without_docker_desktop_annotations(
                image.config.and_then(|config| config.labels),
            ),
        })
    }

    async fn inspect_group_execution(
        &self,
        container_id: &str,
    ) -> Result<DockerGroupExecution, ClientError> {
        let response = bounded_operation(
            "Docker group execution inspect",
            self.docker.inspect_container(container_id, None),
        )
        .await?;
        let config = response.config.unwrap_or_default();
        let mounts = response
            .host_config
            .and_then(|config| config.mounts)
            .unwrap_or_default()
            .into_iter()
            .map(|mount| {
                if mount.typ != Some(MountType::BIND) {
                    return Err(ClientError::new("Docker group mount is not a bind mount"));
                }
                Ok(DockerMount {
                    source: mount
                        .source
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| ClientError::new("Docker group mount has no source"))?,
                    target: mount
                        .target
                        .ok_or_else(|| ClientError::new("Docker group mount has no target"))?,
                    read_only: mount.read_only.unwrap_or(false),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DockerGroupExecution {
            image: config.image.unwrap_or_default(),
            entrypoint: config.entrypoint.unwrap_or_default(),
            argv: config.cmd.unwrap_or_default(),
            environment: config.env.unwrap_or_default(),
            mounts,
        })
    }

    async fn container_stats(
        &self,
        container_id: &str,
    ) -> Result<DockerContainerStats, ClientError> {
        // One-shot skips the daemon's second CPU sample, which callers don't need: they derive
        // rates from their own consecutive readings.
        let options = StatsOptionsBuilder::default()
            .stream(false)
            .one_shot(true)
            .build();
        let response = bounded_operation("Docker container stats", async {
            self.docker
                .stats(container_id, Some(options))
                .next()
                .await
                .transpose()
        })
        .await?
        .ok_or_else(|| ClientError::new("Docker container stats response is empty"))?;
        let memory = response.memory_stats.unwrap_or_default();
        Ok(DockerContainerStats {
            cpu_time_ns: response
                .cpu_stats
                .and_then(|stats| stats.cpu_usage)
                .and_then(|usage| usage.total_usage),
            memory_usage_bytes: memory.usage,
            memory_stat: memory.stats.unwrap_or_default().into_iter().collect(),
        })
    }
}

/// Maps one container listing entry without Docker Desktop annotations, so a listing matches an
/// inspection of the same container.
fn listed_container(container: ContainerSummary) -> DockerContainer {
    let names = container.names.unwrap_or_default();
    DockerContainer {
        id: container.id.unwrap_or_default(),
        name: names
            .first()
            .map(|name| name.trim_start_matches('/').to_owned())
            .unwrap_or_default(),
        name_is_unambiguous: names.len() == 1,
        labels: without_docker_desktop_annotations(container.labels),
    }
}

/// Drops the `desktop.docker.io/` annotations Docker Desktop adds to container listings (port and
/// mount descriptions) but never to an inspected container's labels. A `docker commit` on Docker
/// Desktop bakes them into image labels, which containers created from that image then lack.
fn without_docker_desktop_annotations(
    labels: Option<HashMap<String, String>>,
) -> BTreeMap<String, String> {
    labels
        .unwrap_or_default()
        .into_iter()
        .filter(|(key, _)| !key.starts_with(DOCKER_DESKTOP_ANNOTATION_PREFIX))
        .collect()
}

fn strict_loopback_host_ports(
    ports: HashMap<String, Option<Vec<PortBinding>>>,
) -> Result<BTreeMap<u16, u16>, ClientError> {
    let mut host_ports = BTreeMap::new();
    for (key, bindings) in ports {
        let Some(container_port) = key
            .strip_suffix("/tcp")
            .and_then(|port| port.parse::<u16>().ok())
        else {
            continue;
        };
        let bindings = bindings.ok_or_else(|| {
            ClientError::new(format!(
                "Docker container port {key} must have exactly one host binding"
            ))
        })?;
        let [binding] = bindings.as_slice() else {
            return Err(ClientError::new(format!(
                "Docker container port {key} must have exactly one host binding"
            )));
        };
        if binding.host_ip.as_deref() != Some("127.0.0.1") {
            return Err(ClientError::new(format!(
                "Docker container port {key} is not bound exclusively to IPv4 loopback"
            )));
        }
        let host_port = binding
            .host_port
            .as_deref()
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                ClientError::new(format!(
                    "Docker container port {key} has a malformed host port"
                ))
            })?;
        if host_ports.insert(container_port, host_port).is_some() {
            return Err(ClientError::new(format!(
                "Docker container port {container_port}/tcp has ambiguous bindings"
            )));
        }
    }
    Ok(host_ports)
}

/// Exact owned Docker source request passed only within the Docker adapter boundary.
#[derive(Clone, Debug)]
pub(super) struct DockerLogRequest {
    pub(super) container_id: String,
    pub(super) follow: bool,
    pub(super) since: Option<SystemTime>,
    pub(super) until: Option<SystemTime>,
    /// Newest lines to return; `None` returns all output.
    pub(super) tail: Option<u16>,
}

pub(super) type DockerLogStream =
    Pin<Box<dyn Stream<Item = Result<DockerLogOutput, ClientError>> + Send>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerLogOutput {
    pub(super) stream: DockerLogStreamKind,
    pub(super) timestamp: SystemTime,
    pub(super) content: Vec<u8>,
    pub(super) truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DockerLogStreamKind {
    Stdout,

    Stderr,
}

#[cfg(test)]
impl DockerLogOutput {
    pub(super) fn stdout(timestamp: SystemTime, content: Vec<u8>) -> Self {
        Self {
            stream: DockerLogStreamKind::Stdout,
            timestamp,
            content,
            truncated: false,
        }
    }

    pub(super) fn stderr(timestamp: SystemTime, content: Vec<u8>) -> Self {
        Self {
            stream: DockerLogStreamKind::Stderr,
            timestamp,
            content,
            truncated: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerContainer {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) name_is_unambiguous: bool,
    pub(super) labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ContainerInspection {
    pub(super) running: bool,
    pub(super) exit_status: Option<i64>,
    pub(super) process_id: Option<u32>,
    pub(super) host_ports: BTreeMap<u16, u16>,
    /// Host sources of the container's bind mounts.
    pub(super) bind_sources: Vec<std::path::PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GroupContainerInspection {
    pub(super) running: bool,
    pub(super) process_id: Option<u32>,
    pub(super) host_bindings: BTreeMap<String, Option<Vec<GroupPortBinding>>>,
    pub(super) private_address: Option<std::net::Ipv4Addr>,
    pub(super) networks: BTreeMap<String, GroupNetworkAttachment>,
}

/// Docker-reported container labels with their immutable source image labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GroupContainerOwnership {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) labels: BTreeMap<String, String>,
    pub(super) image_labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerGroupExecution {
    pub(super) image: String,
    pub(super) entrypoint: Vec<String>,
    pub(super) argv: Vec<String>,
    pub(super) environment: Vec<String>,
    pub(super) mounts: Vec<DockerMount>,
}

/// One container's resource figures as the Docker Engine reports them. A container that is not
/// running reports none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DockerContainerStats {
    /// Cumulative CPU time of the container's processes.
    pub(super) cpu_time_ns: Option<u64>,

    /// Memory the container's control group charges, file cache included.
    pub(super) memory_usage_bytes: Option<u64>,

    /// The control group's memory statistics, whose keys differ between cgroup v1 and v2.
    pub(super) memory_stat: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GroupPortBinding {
    pub(super) host: Option<String>,
    pub(super) port: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GroupNetworkAttachment {
    pub(super) network_id: Option<String>,
    pub(super) aliases: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerNetworkSummary {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerNetwork {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) labels: BTreeMap<String, String>,
    pub(super) internal: bool,
    pub(super) attachable: bool,
    pub(super) driver: String,
    pub(super) attached_container_ids: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ClientError {
    #[error("Docker Engine API request failed: {0}")]
    Docker(#[from] bollard::errors::Error),

    #[error("Docker Engine operation timed out: {operation}")]
    Timeout {
        operation: &'static str,
        #[source]
        source: tokio::time::error::Elapsed,
    },

    #[error("Invalid Docker Engine response: {0}")]
    InvalidResponse(#[source] anyhow::Error),
}

impl ClientError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self::InvalidResponse(anyhow!(message.into()))
    }

    #[cfg(test)]
    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self::Docker(bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: message.into(),
        })
    }

    pub(super) fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Docker(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        )
    }

    pub(super) fn is_missing_image(&self) -> bool {
        self.is_not_found()
            && self
                .to_string()
                .to_ascii_lowercase()
                .contains("no such image")
    }

    /// Reports a container start the Docker Engine refused because a requested host port is
    /// already held, by another container or by a host process.
    pub(super) fn is_port_in_use(&self) -> bool {
        let Self::Docker(bollard::errors::Error::DockerResponseServerError { message, .. }) = self
        else {
            return false;
        };
        let message = message.to_ascii_lowercase();
        message.contains("port is already allocated") || message.contains("address already in use")
    }

    pub(super) fn is_not_modified(&self) -> bool {
        matches!(
            self,
            Self::Docker(bollard::errors::Error::DockerResponseServerError {
                status_code: 304,
                ..
            })
        )
    }
}

const MAX_DOCKER_LOG_RECORD_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct DockerLogFramer {
    stdout: PendingDockerLogRecord,
    stderr: PendingDockerLogRecord,
}

impl DockerLogFramer {
    fn push(&mut self, output: LogOutput) -> Result<Vec<DockerLogOutput>, ClientError> {
        let (stream, bytes) = log_output_parts(output)?;
        let pending = match stream {
            DockerLogStreamKind::Stdout => &mut self.stdout,
            DockerLogStreamKind::Stderr => &mut self.stderr,
        };
        pending.push(stream, &bytes)
    }

    fn finish(&mut self) -> Vec<DockerLogOutput> {
        let mut output = Vec::new();
        for (stream, pending) in [
            (DockerLogStreamKind::Stdout, &mut self.stdout),
            (DockerLogStreamKind::Stderr, &mut self.stderr),
        ] {
            if !pending.pending.is_empty()
                && let Ok(entry) = pending.finish(stream)
            {
                output.push(entry);
            }
        }
        output
    }
}

#[derive(Default)]
struct PendingDockerLogRecord {
    pending: Vec<u8>,
    discarding: bool,
}

impl PendingDockerLogRecord {
    fn push(
        &mut self,
        stream: DockerLogStreamKind,
        mut bytes: &[u8],
    ) -> Result<Vec<DockerLogOutput>, ClientError> {
        let mut output = Vec::new();
        while !bytes.is_empty() {
            if self.discarding {
                let Some(delimiter) = bytes.iter().position(|byte| *byte == b'\n') else {
                    return Ok(output);
                };
                self.pending.push(b'\n');
                output.push(self.take(stream, true)?);
                self.discarding = false;
                bytes = &bytes[delimiter + 1..];
                continue;
            }

            if let Some(delimiter) = bytes.iter().position(|byte| *byte == b'\n') {
                let record_len = delimiter + 1;
                if self.pending.len() + record_len <= MAX_DOCKER_LOG_RECORD_BYTES {
                    self.pending.extend_from_slice(&bytes[..record_len]);
                    output.push(self.take(stream, false)?);
                } else {
                    self.append_prefix(&bytes[..record_len]);
                    self.pending.push(b'\n');
                    output.push(self.take(stream, true)?);
                }
                bytes = &bytes[record_len..];
            } else {
                let remaining =
                    (MAX_DOCKER_LOG_RECORD_BYTES - 1).saturating_sub(self.pending.len());
                self.append_prefix(bytes);
                self.discarding = bytes.len() > remaining;
                return Ok(output);
            }
        }
        Ok(output)
    }

    fn finish(&mut self, stream: DockerLogStreamKind) -> Result<DockerLogOutput, ClientError> {
        self.take(stream, self.discarding)
    }

    fn append_prefix(&mut self, bytes: &[u8]) {
        let remaining = (MAX_DOCKER_LOG_RECORD_BYTES - 1).saturating_sub(self.pending.len());
        self.pending
            .extend_from_slice(&bytes[..remaining.min(bytes.len())]);
    }

    fn take(
        &mut self,
        stream: DockerLogStreamKind,
        truncated: bool,
    ) -> Result<DockerLogOutput, ClientError> {
        let mut entry = parse_log_line(stream, std::mem::take(&mut self.pending))?;
        entry.truncated = truncated;
        Ok(entry)
    }
}

fn log_output_parts(output: LogOutput) -> Result<(DockerLogStreamKind, Vec<u8>), ClientError> {
    match output {
        LogOutput::StdOut { message } | LogOutput::Console { message } => {
            Ok((DockerLogStreamKind::Stdout, message.to_vec()))
        }
        LogOutput::StdErr { message } => Ok((DockerLogStreamKind::Stderr, message.to_vec())),
        LogOutput::StdIn { .. } => Err(ClientError::new("Docker logs returned stdin data")),
    }
}

fn parse_log_line(
    stream: DockerLogStreamKind,
    message: Vec<u8>,
) -> Result<DockerLogOutput, ClientError> {
    let separator = message
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| ClientError::new("Docker logs returned an entry without a timestamp"))?;
    let (timestamp, content) = message.split_at(separator);
    let timestamp = std::str::from_utf8(timestamp)
        .map_err(|_| ClientError::new("Docker logs returned a non-UTF-8 timestamp"))?;
    let timestamp = humantime::parse_rfc3339(timestamp)
        .map_err(|_| ClientError::new("Docker logs returned an invalid timestamp"))?;
    Ok(DockerLogOutput {
        stream,
        timestamp,
        content: content[1..].to_vec(),
        truncated: false,
    })
}

fn docker_timestamp(timestamp: SystemTime) -> i32 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i32::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

fn owned_network_inspection_options() -> bollard::query_parameters::InspectNetworkOptions {
    InspectNetworkOptionsBuilder::default()
        .verbose(true)
        .build()
}

async fn list_network_summaries_by_label(
    docker: &Docker,
    label: String,
) -> Result<Vec<DockerNetworkSummary>, ClientError> {
    let options = ListNetworksOptionsBuilder::default()
        .filters(&HashMap::from([("label", vec![label])]))
        .build();
    bounded_operation(
        "Docker group network list",
        docker.list_networks(Some(options)),
    )
    .await
    .map(|networks| {
        networks
            .into_iter()
            .map(|network| DockerNetworkSummary {
                id: network.id.unwrap_or_default(),
                name: network.name.unwrap_or_default(),
                labels: network.labels.unwrap_or_default().into_iter().collect(),
            })
            .collect()
    })
}

/// Bounds one Docker Engine request, so a daemon that stops answering fails the operation instead
/// of hanging it.
pub(super) async fn bounded_operation<T, F>(
    operation: &'static str,
    future: F,
) -> Result<T, ClientError>
where
    F: Future<Output = Result<T, bollard::errors::Error>>,
{
    real_time::bound(DOCKER_OPERATION_TIMEOUT, future)
        .await
        .map_err(|source| ClientError::Timeout { operation, source })?
        .map_err(ClientError::Docker)
}

/// Maps prepared accelerator device requests into the Docker Engine create body form.
fn docker_device_requests(requests: &[DockerDeviceRequest]) -> Vec<DeviceRequest> {
    requests
        .iter()
        .map(|device| DeviceRequest {
            driver: Some(device.driver.clone()),
            count: None,
            device_ids: Some(device.device_ids.clone()),
            capabilities: device.capabilities.clone(),
            options: None,
        })
        .collect()
}

fn individual_container_create_body(request: &DockerCreateRequest) -> ContainerCreateBody {
    let port_bindings = request
        .ports
        .iter()
        .map(|port| {
            (
                format!("{}/tcp", port.container_port),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: None,
                }]),
            )
        })
        .collect();
    let mounts = request
        .mounts
        .iter()
        .map(|mount| Mount {
            target: Some(mount.target.clone()),
            source: Some(mount.source.to_string_lossy().into_owned()),
            typ: Some(MountType::BIND),
            read_only: Some(mount.read_only),
            ..Mount::default()
        })
        .collect();
    let device_requests = docker_device_requests(&request.device_requests);

    ContainerCreateBody {
        image: Some(request.image.clone()),
        entrypoint: (!request.entrypoint.is_empty()).then(|| request.entrypoint.clone()),
        cmd: Some(request.argv.clone()),
        labels: Some(request.labels.clone().into_iter().collect()),
        exposed_ports: Some(
            request
                .ports
                .iter()
                .map(|port| format!("{}/tcp", port.container_port))
                .collect(),
        ),
        env: Some(request.environment.clone()),
        host_config: Some(HostConfig {
            port_bindings: Some(port_bindings),
            mounts: Some(mounts),
            device_requests: Some(device_requests),
            privileged: Some(false),
            network_mode: Some("bridge".to_owned()),
            ipc_mode: Some(request.ipc_mode.as_docker_value().to_owned()),
            publish_all_ports: Some(false),
            binds: Some(Vec::new()),
            cap_add: Some(Vec::new()),
            devices: Some(Vec::new()),
            security_opt: Some(Vec::new()),
            volumes_from: Some(Vec::new()),
            log_config: Some(workload_log_config()),
            ..HostConfig::default()
        }),
        shell: Some(Vec::new()),
        ..ContainerCreateBody::default()
    }
}

fn group_container_create_body(request: &DockerGroupContainerRequest) -> ContainerCreateBody {
    let port_bindings = request
        .ports
        .iter()
        .filter_map(|port| {
            port.publication.map(|address| {
                (
                    format!("{}/tcp", port.container_port),
                    Some(vec![PortBinding {
                        host_ip: Some(address.ip().to_string()),
                        host_port: Some(address.port().to_string()),
                    }]),
                )
            })
        })
        .collect();
    let mounts = request
        .mounts
        .iter()
        .map(|mount| Mount {
            target: Some(mount.target.clone()),
            source: Some(mount.source.to_string_lossy().into_owned()),
            typ: Some(MountType::BIND),
            read_only: Some(mount.read_only),
            ..Mount::default()
        })
        .collect();
    let device_requests = docker_device_requests(&request.device_requests);

    ContainerCreateBody {
        image: Some(request.image.clone()),
        entrypoint: (!request.entrypoint.is_empty()).then(|| request.entrypoint.clone()),
        cmd: (!request.argv.is_empty()).then(|| request.argv.clone()),
        labels: Some(request.labels.clone().into_iter().collect()),
        env: Some(request.environment.clone()),
        exposed_ports: Some(
            request
                .ports
                .iter()
                .map(|port| format!("{}/tcp", port.container_port))
                .collect(),
        ),
        host_config: Some(HostConfig {
            network_mode: Some(request.network_name.clone()),
            ipc_mode: Some("private".to_owned()),
            extra_hosts: Some(request.extra_hosts.clone()),
            port_bindings: Some(port_bindings),
            mounts: Some(mounts),
            device_requests: Some(device_requests),
            privileged: Some(false),
            publish_all_ports: Some(false),
            binds: Some(Vec::new()),
            cap_add: Some(Vec::new()),
            devices: Some(Vec::new()),
            security_opt: Some(Vec::new()),
            volumes_from: Some(Vec::new()),
            log_config: Some(workload_log_config()),
            ..HostConfig::default()
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(HashMap::from([(
                request.network_name.clone(),
                EndpointSettings {
                    aliases: Some(request.aliases.clone()),
                    ..EndpointSettings::default()
                },
            )])),
        }),
        shell: Some(Vec::new()),
        ..ContainerCreateBody::default()
    }
}

fn workload_log_config() -> HostConfigLogConfig {
    HostConfigLogConfig {
        typ: Some(WORKLOAD_LOG_DRIVER.to_owned()),
        config: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, future::pending};

    use anyhow::anyhow;
    use bollard::{
        container::LogOutput,
        models::{ContainerSummary, DeviceRequest, HostConfigLogConfig, PortBinding},
    };
    use bytes::Bytes;

    use super::{
        CONTAINER_EXISTENCE_INSPECTION, ClientError, DockerClient, DockerContainer,
        DockerCreateRequest, DockerDeviceRequest, DockerGroupContainerRequest, DockerLogFramer,
        DockerLogStreamKind, GroupContainerOwnership, LocalDockerClient, bounded_operation,
        group_container_create_body, individual_container_create_body, listed_container,
        owned_network_inspection_options, strict_loopback_host_ports,
        without_docker_desktop_annotations,
    };
    use crate::docker::request::DockerIpcMode;

    #[test]
    fn listed_container_drops_docker_desktop_listing_annotations() {
        let listed = listed_container(ContainerSummary {
            id: Some("container-id".to_owned()),
            names: Some(vec!["/dotcore_site_model_1".to_owned()]),
            labels: Some(
                [
                    ("com.dotcore.site", "site"),
                    ("desktop.docker.io/ports.scheme", "v2"),
                    ("desktop.docker.io/ports/8080/tcp", "127.0.0.1:"),
                    ("desktop.docker.io/mounts/0/Source", "/Users/dotcore/data"),
                ]
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            ),
            ..ContainerSummary::default()
        });

        assert_eq!(
            listed,
            DockerContainer {
                id: "container-id".to_owned(),
                name: "dotcore_site_model_1".to_owned(),
                name_is_unambiguous: true,
                labels: [("com.dotcore.site".to_owned(), "site".to_owned())].into(),
            }
        );
    }

    #[test]
    fn committed_image_labels_drop_docker_desktop_annotations() {
        let labels = without_docker_desktop_annotations(Some(
            [
                ("com.dotcore.test.site", "site"),
                ("desktop.docker.io/ports.scheme", "v2"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        ));

        assert_eq!(
            labels,
            [("com.dotcore.test.site".to_owned(), "site".to_owned())].into()
        );
        assert!(without_docker_desktop_annotations(None).is_empty());
    }

    #[test]
    fn individual_container_body_emits_explicit_ipc_isolation_and_local_log_driver() {
        for (mode, expected) in [
            (DockerIpcMode::Private, "private"),
            (DockerIpcMode::Host, "host"),
        ] {
            let body = individual_container_create_body(&DockerCreateRequest {
                name: "workload".to_owned(),
                image: "example.test/workload@sha256:abc".to_owned(),
                entrypoint: vec!["serve".to_owned()],
                argv: Vec::new(),
                labels: std::collections::BTreeMap::new(),
                ports: Vec::new(),
                mounts: Vec::new(),
                device_requests: Vec::new(),
                environment: Vec::new(),
                ipc_mode: mode,
            });

            let host_config = body.host_config.expect("individual container HostConfig");
            assert_eq!(host_config.ipc_mode.as_deref(), Some(expected));
            assert_eq!(host_config.network_mode.as_deref(), Some("bridge"));
            assert_eq!(host_config.privileged, Some(false));
            assert_eq!(
                host_config.log_config,
                Some(HostConfigLogConfig {
                    typ: Some("local".to_owned()),
                    config: None,
                })
            );
        }
    }

    #[test]
    fn application_group_container_keeps_private_ipc_local_log_driver_and_host_entries() {
        let body = group_container_create_body(&DockerGroupContainerRequest {
            name: "application".to_owned(),
            image: "example.test/application@sha256:abc".to_owned(),
            entrypoint: Vec::new(),
            argv: Vec::new(),
            labels: std::collections::BTreeMap::new(),
            ports: Vec::new(),
            mounts: Vec::new(),
            device_requests: Vec::new(),
            environment: Vec::new(),
            network_name: "application-network".to_owned(),
            aliases: vec!["service".to_owned()],
            extra_hosts: vec!["manager.example.ts.net:100.64.0.7".to_owned()],
        });

        let host_config = body.host_config.expect("application container HostConfig");
        assert_eq!(host_config.ipc_mode.as_deref(), Some("private"));
        assert_eq!(
            host_config.network_mode.as_deref(),
            Some("application-network")
        );
        assert_eq!(
            host_config.extra_hosts,
            Some(vec!["manager.example.ts.net:100.64.0.7".to_owned()])
        );
        assert_eq!(
            host_config.log_config,
            Some(HostConfigLogConfig {
                typ: Some("local".to_owned()),
                config: None,
            })
        );
    }

    #[test]
    fn application_group_container_passes_device_requests_to_the_host_config() {
        let body = group_container_create_body(&DockerGroupContainerRequest {
            name: "application".to_owned(),
            image: "example.test/application@sha256:abc".to_owned(),
            entrypoint: Vec::new(),
            argv: Vec::new(),
            labels: std::collections::BTreeMap::new(),
            ports: Vec::new(),
            mounts: Vec::new(),
            device_requests: vec![DockerDeviceRequest {
                driver: "cdi".to_owned(),
                device_ids: vec!["nvidia.com/gpu=0".to_owned()],
                capabilities: None,
            }],
            environment: Vec::new(),
            network_name: "application-network".to_owned(),
            aliases: vec!["service".to_owned()],
            extra_hosts: Vec::new(),
        });

        let host_config = body.host_config.expect("application container HostConfig");
        assert_eq!(
            host_config.device_requests,
            Some(vec![DeviceRequest {
                driver: Some("cdi".to_owned()),
                count: None,
                device_ids: Some(vec!["nvidia.com/gpu=0".to_owned()]),
                capabilities: None,
                options: None,
            }])
        );
    }

    #[test]
    fn strict_endpoint_inspection_accepts_exactly_one_loopback_binding() {
        let ports = strict_loopback_host_ports(std::collections::HashMap::from([(
            "8080/tcp".to_owned(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_owned()),
                host_port: Some("49152".to_owned()),
            }]),
        )]))
        .expect("strict loopback binding");

        assert_eq!(ports, std::collections::BTreeMap::from([(8080, 49152)]));
    }

    #[test]
    fn strict_endpoint_inspection_rejects_missing_and_ambiguous_bindings() {
        for bindings in [None, Some(Vec::new())] {
            let error = strict_loopback_host_ports(std::collections::HashMap::from([(
                "8080/tcp".to_owned(),
                bindings,
            )]))
            .expect_err("missing Docker binding");

            assert_eq!(
                error.to_string(),
                "Invalid Docker Engine response: Docker container port 8080/tcp must have exactly one host binding"
            );
        }

        let binding = Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_owned()),
            host_port: Some("49152".to_owned()),
        }]);
        let error = strict_loopback_host_ports(std::collections::HashMap::from([
            ("8080/tcp".to_owned(), binding.clone()),
            ("08080/tcp".to_owned(), binding),
        ]))
        .expect_err("equivalent Docker port keys");

        assert_eq!(
            error.to_string(),
            "Invalid Docker Engine response: Docker container port 8080/tcp has ambiguous bindings"
        );
    }

    #[test]
    fn strict_endpoint_inspection_rejects_multiple_bindings() {
        for bindings in [
            vec![
                PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: Some("49152".to_owned()),
                },
                PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: Some("49153".to_owned()),
                },
            ],
            vec![
                PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: Some("49152".to_owned()),
                },
                PortBinding {
                    host_ip: Some("0.0.0.0".to_owned()),
                    host_port: Some("49152".to_owned()),
                },
            ],
        ] {
            let error = strict_loopback_host_ports(std::collections::HashMap::from([(
                "8080/tcp".to_owned(),
                Some(bindings),
            )]))
            .expect_err("multiple Docker bindings");

            assert_eq!(
                error.to_string(),
                "Invalid Docker Engine response: Docker container port 8080/tcp must have exactly one host binding"
            );
        }
    }

    #[test]
    fn strict_endpoint_inspection_rejects_wildcard_and_non_loopback_bindings() {
        for host in [
            None,
            Some(""),
            Some("0.0.0.0"),
            Some("::"),
            Some("192.0.2.1"),
        ] {
            let error = strict_loopback_host_ports(std::collections::HashMap::from([(
                "8080/tcp".to_owned(),
                Some(vec![PortBinding {
                    host_ip: host.map(str::to_owned),
                    host_port: Some("49152".to_owned()),
                }]),
            )]))
            .expect_err("non-loopback Docker binding");

            assert_eq!(
                error.to_string(),
                "Invalid Docker Engine response: Docker container port 8080/tcp is not bound exclusively to IPv4 loopback"
            );
        }
    }

    #[test]
    fn docker_log_framer_reassembles_timestamped_records_split_across_chunks() {
        let mut framer = DockerLogFramer::default();
        assert!(
            framer
                .push(LogOutput::StdOut {
                    message: Bytes::from_static(b"2024-01-01T00:")
                })
                .expect("first chunk")
                .is_empty()
        );
        let records = framer
            .push(LogOutput::StdOut {
                message: Bytes::from_static(b"00:00Z first\n"),
            })
            .expect("second chunk");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].stream, DockerLogStreamKind::Stdout);
        assert_eq!(records[0].content, b"first\n");
        assert!(!records[0].truncated);
    }

    #[test]
    fn docker_log_framer_bounds_newline_free_chunks_until_a_delimiter() {
        let mut framer = DockerLogFramer::default();
        let timestamp = b"2024-01-01T00:00:00Z ";
        let prefix = b"x".repeat(64 * 1024);

        assert!(
            framer
                .push(LogOutput::StdOut {
                    message: Bytes::from([timestamp, &prefix[..32 * 1024]].concat()),
                })
                .expect("first oversized chunk")
                .is_empty()
        );
        assert!(
            framer
                .push(LogOutput::StdOut {
                    message: Bytes::from(prefix[32 * 1024..].to_vec()),
                })
                .expect("second oversized chunk")
                .is_empty()
        );
        assert!(framer.stdout.pending.len() <= 64 * 1024);
        assert!(framer.stdout.discarding);

        let records = framer
            .push(LogOutput::StdOut {
                message: Bytes::from_static(b"discarded\n2024-01-01T00:00:01Z next\n"),
            })
            .expect("delimiter and following record");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].stream, DockerLogStreamKind::Stdout);
        assert_eq!(
            records[0].timestamp,
            humantime::parse_rfc3339("2024-01-01T00:00:00Z").unwrap()
        );
        assert_eq!(
            records[0].content,
            [b"x".repeat(64 * 1024 - timestamp.len() - 1), vec![b'\n']].concat()
        );
        assert!(records[0].truncated);
        assert_eq!(records[1].content, b"next\n");
        assert!(!records[1].truncated);
    }

    #[test]
    fn docker_log_framer_emits_a_bounded_truncated_record_at_eof() {
        let mut framer = DockerLogFramer::default();
        let timestamp = b"2024-01-01T00:00:00Z ";
        let prefix = b"x".repeat(64 * 1024);
        assert!(
            framer
                .push(LogOutput::StdErr {
                    message: Bytes::from([timestamp.as_slice(), prefix.as_slice()].concat()),
                })
                .expect("oversized chunk")
                .is_empty()
        );
        assert!(framer.stderr.pending.len() <= 64 * 1024);
        assert!(framer.stderr.discarding);

        let records = framer.finish();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].stream, DockerLogStreamKind::Stderr);
        assert_eq!(
            records[0].timestamp,
            humantime::parse_rfc3339("2024-01-01T00:00:00Z").unwrap()
        );
        assert_eq!(
            records[0].content,
            b"x".repeat(64 * 1024 - timestamp.len() - 1)
        );
        assert!(records[0].truncated);
    }

    #[test]
    fn docker_client_boundary_exposes_no_oci_acquisition_operation() {
        let production_source = include_str!("client.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("Docker client production source");

        assert!(!production_source.contains("fn pull("));
        assert!(!production_source.contains("create_image"));
        assert!(!production_source.contains("OciPreparer"));
    }

    #[test]
    fn client_error_keeps_its_original_source() {
        let error = ClientError::InvalidResponse(anyhow!("daemon rejected the image"));

        assert_eq!(
            error.to_string(),
            "Invalid Docker Engine response: daemon rejected the image"
        );
        assert_eq!(
            error.source().expect("original Docker error").to_string(),
            "daemon rejected the image"
        );
    }

    #[test]
    fn owned_network_inspection_requests_attachment_evidence() {
        assert!(owned_network_inspection_options().verbose);
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_operation_timeout_keeps_container_existence_label() {
        let error = bounded_operation(
            "Docker container existence inspection",
            pending::<Result<(), bollard::errors::Error>>(),
        )
        .await
        .expect_err("pending Docker operation should time out");

        assert_eq!(
            error.to_string(),
            "Docker Engine operation timed out: Docker container existence inspection"
        );
        let ClientError::Timeout { operation, .. } = &error else {
            unreachable!("constructed Docker timeout error");
        };
        assert_eq!(*operation, CONTAINER_EXISTENCE_INSPECTION);
        assert!(matches!(
            error.source(),
            Some(source) if source.is::<tokio::time::error::Elapsed>()
        ));
    }

    #[tokio::test]
    async fn group_ownership_reads_image_labels_by_image_id() {
        let directory = tempfile::tempdir().expect("socket directory");
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind fake daemon");
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                tokio::spawn(serve_fake_daemon(stream));
            }
        });
        let client = LocalDockerClient {
            docker: bollard::Docker::connect_with_unix(
                socket.to_str().expect("socket path"),
                5,
                bollard::API_DEFAULT_VERSION,
            )
            .expect("connect fake daemon"),
        };

        let ownership = client
            .inspect_group_container_ownership("container-id")
            .await
            .expect("ownership of a container whose creation-time tag is gone");

        assert_eq!(
            ownership,
            GroupContainerOwnership {
                id: "container-id".to_owned(),
                name: "unit".to_owned(),
                labels: [
                    ("com.dotcore.test.image".to_owned(), "inherited".to_owned()),
                    ("com.dotcore.test.site".to_owned(), "site".to_owned()),
                ]
                .into(),
                image_labels: [("com.dotcore.test.image".to_owned(), "inherited".to_owned())]
                    .into(),
            }
        );
    }

    /// Answers the container inspect and the by-ID image inspect; the creation-time tag is unknown.
    async fn serve_fake_daemon(mut stream: tokio::net::UnixStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut buffer = vec![0; 8192];
        let read = stream.read(&mut buffer).await.expect("read request");
        let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
        let path = request.split_whitespace().nth(1).unwrap_or_default();
        let (status, body) = if path.ends_with("/containers/container-id/json") {
            (
                "200 OK",
                r#"{"Id":"container-id","Name":"/unit","Image":"sha256:abc","Config":{"Image":"example.test/app:removed-tag","Labels":{"com.dotcore.test.image":"inherited","com.dotcore.test.site":"site"}}}"#,
            )
        } else if path.ends_with("/images/sha256:abc/json") {
            (
                "200 OK",
                r#"{"Id":"sha256:abc","Config":{"Labels":{"com.dotcore.test.image":"inherited"}}}"#,
            )
        } else {
            ("404 Not Found", r#"{"message":"No such image"}"#)
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    }
}
