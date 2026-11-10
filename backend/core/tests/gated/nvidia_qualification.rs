//! Shared harness for the NVIDIA qualification suites: the temporary qualification site on the
//! native Docker platform, GPU observation through the backend's own hardware collector, the
//! free-memory preflight, and the proof that a stopped workload released the GPU.
//!
//! The site loads the workspace master catalog as is, so its development-source engines are built
//! by the hosting node from the workspace development source, which a run must supply. The harness
//! evaluates the same source independently to check the engine pin the node records and the
//! Docker image its loader tags.
//!
//! The suites share the GPU with other workloads: they need only enough free memory, read from the
//! pool the device allocates from (its dedicated memory, or the host's available memory on
//! unified-memory devices such as the NVIDIA GB10), and judge a workload released once every GPU
//! process it ran is gone, whatever else runs on the device. A suite whose engine release approves
//! no artifact for the native platform skips instead of failing.

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow};
use bollard::{
    Docker,
    models::SystemInfo,
    query_parameters::{ListContainersOptionsBuilder, RemoveContainerOptionsBuilder},
};
use dotcore::{composition::DEV_SOURCE_FLAKE_ENV, testing::host_reserve_bytes};
use dotcore_catalog::{Catalog, CatalogModelId, OciArtifactPolicy};
use dotcore_domain::{
    ExecutionPlatform, ExecutionPlatformError, SitePaths,
    clock::SystemClock,
    hardware_observation::{
        AcceleratorProcessObservation, AcceleratorVendorProcessResult, HardwareSnapshotSource,
        NodeHardwareObservation,
    },
    nix::DotcoreSources,
};
use dotcore_node_hardware::hardware::{
    SystemHardwareSnapshotCollector, SystemNvidiaAcceleratorAdapter,
};
use dotcore_proto::dotcore::{
    capacity::v1::{MemoryPoolKind, MemorySegmentSource},
    catalog::v1::{
        EngineArtifact, EngineParameterValue, engine_artifact::Kind as EngineArtifactKind,
        engine_parameter_value, nix_artifact::Provides,
    },
    deployment::v1::{
        ArtifactPreparationPolicy, ExactPlacement, NodeResolvedLaunchIdentity,
        accelerator_memory_observation,
    },
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
        EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
        ListModelDeploymentsRequest, ListModelProfileCompatibilityRequest,
        LocalModelDeploymentConfiguration, ModelDeployment, ModelDeploymentStatus,
        RemoveModelDeploymentRequest, ResolvedEngineArtifact, StartingTotalContextSource,
        create_model_deployment_request,
        model_deployment_service_client::ModelDeploymentServiceClient,
    },
    observation::v1::{
        AcceleratorMemoryArchitecture, AcceleratorVendor, accelerator_observation, host_observation,
    },
    site::v1::{HardwareObservationConfiguration, Site},
};
use dotcore_runtime_backends::docker::OWNERSHIP_SCHEMA;
use fs4::fs_std::FileExt;

/// One lock for every suite, since each needs the GPU to itself.
const LOCK_FILE: &str = "dotcore-nvidia-qualification.lock";
const NVIDIA_ACCELERATOR: &str = "nvidia";
const LABEL_SITE: &str = "com.dotcore.site";
const LABEL_CORRELATION: &str = "com.dotcore.workload.correlation";
const LABEL_SCHEMA: &str = "com.dotcore.runtime.schema";
const LABEL_MANAGED: &str = "com.dotcore.managed";
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Selection key under which a deployment records its engine's node-resolved launch identity.
const ENGINE_SELECTION_KEY: &str = "engine";
/// Covers a cold engine preparation, a first download of the weights and the load itself.
const READY_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

pub type ModelClient = ModelDeploymentServiceClient<tonic::transport::Channel>;

/// A temporary qualification site copied from a fixture, with its execution targets on the native
/// Docker platform, the master and site catalog view, the development source its backends build
/// engines from, and the host-wide qualification lock.
pub struct QualificationSite {
    pub root: PathBuf,
    pub configuration: SitePaths,
    pub catalog: Catalog,
    pub platform: ExecutionPlatform,

    /// Dotcore sources every backend of the site is composed with, so its node can build the
    /// catalog's development-source engines.
    pub dotcore_sources: DotcoreSources,
    site_id: String,
    _lock: File,
}

impl QualificationSite {
    /// Reads the workspace development source, takes the qualification lock and materializes
    /// `backend/fixtures/<fixture>` on the native Docker platform, with model artifacts in the
    /// shared `DOTCORE_CACHE_DIR`.
    pub async fn new(fixture: &str) -> anyhow::Result<Self> {
        let dotcore_sources = development_sources()?;
        let platform = native_runtime_platform().await?;
        let lock = acquire_lock()?;
        let root = crate::support::canonical_temp_dir().join(format!(
            "dotcore-{fixture}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("read system clock")?
                .as_nanos()
        ));
        fs::create_dir(&root).context("create temporary qualification site")?;
        let prepared = (|| -> anyhow::Result<(SitePaths, Catalog, String)> {
            let site_id = materialize_site(fixture, &platform, &root)?;
            let configuration = SitePaths::from_values(
                root.to_str(),
                master_catalog_root().to_str(),
                shared_cache_root()?.to_str(),
            )
            .context("build qualification site configuration")?;
            Ok((configuration, load_catalog(&root)?, site_id))
        })();
        match prepared {
            Ok((configuration, catalog, site_id)) => Ok(Self {
                root,
                configuration,
                catalog,
                platform,
                dotcore_sources,
                site_id,
                _lock: lock,
            }),
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                Err(error)
            }
        }
    }

    /// Force-removes the site's leftover Docker workloads and deletes the temporary site.
    pub async fn cleanup(self) -> anyhow::Result<()> {
        let mut errors = Vec::new();
        match Docker::connect_with_local_defaults() {
            Ok(docker) => {
                let filters = HashMap::from([(
                    "label",
                    vec![
                        format!("{LABEL_MANAGED}=true"),
                        format!("{LABEL_SITE}={}", self.site_id),
                        format!("{LABEL_SCHEMA}={OWNERSHIP_SCHEMA}"),
                    ],
                )]);
                let options = ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&filters)
                    .build();
                match docker.list_containers(Some(options)).await {
                    Ok(containers) => {
                        for id in containers.into_iter().filter_map(|container| container.id) {
                            let options =
                                RemoveContainerOptionsBuilder::default().force(true).build();
                            if let Err(error) = docker.remove_container(&id, Some(options)).await {
                                errors.push(format!("remove leaked workload {id}: {error}"));
                            }
                        }
                    }
                    Err(error) => errors.push(format!("list leaked workloads: {error}")),
                }
            }
            Err(error) => errors.push(format!("connect Docker for cleanup: {error}")),
        }
        if let Err(error) = fs::remove_dir_all(&self.root)
            && error.kind() != io::ErrorKind::NotFound
        {
            errors.push(format!("remove temporary qualification site: {error}"));
        }
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }

    /// Returns why the profile cannot run here when its engine release approves no NVIDIA
    /// artifact for the native platform, so the suite can skip instead of failing.
    pub fn unsupported_platform_reason(
        &self,
        model_id: &str,
        profile_id: &str,
    ) -> anyhow::Result<Option<String>> {
        unsupported_platform_reason(&self.catalog, model_id, profile_id, &self.platform)
    }
}

/// Copies `backend/fixtures/<fixture>` into `root`: its node binding, its site catalog when present,
/// and its site with every execution target on `platform`, sampling hardware every second.
/// Returns the site id.
pub fn materialize_site(
    fixture: &str,
    platform: &ExecutionPlatform,
    root: &Path,
) -> anyhow::Result<String> {
    let source = fixtures_root().join(fixture);
    let mut site: Site = dotcore_proto::yaml::from_file(&source.join("site.yaml"))
        .with_context(|| format!("read {fixture} site"))?;
    for target in &mut site.execution_targets {
        target.platform = Some(platform.to_oci_platform());
    }
    // Waits for a fresh GPU attribution or memory sample last up to one sampling interval.
    site.hardware_observation = Some(HardwareObservationConfiguration {
        refresh_interval_seconds: Some(1),
    });
    let declaration = dotcore_proto::yaml::to_string(&site)
        .with_context(|| format!("serialize {fixture} site"))?;
    fs::write(root.join("site.yaml"), declaration)
        .with_context(|| format!("write {fixture} site"))?;
    fs::copy(source.join("node.yaml"), root.join("node.yaml"))
        .with_context(|| format!("copy {fixture} node binding"))?;
    if source.join("catalog").is_dir() {
        copy_directory(&source.join("catalog"), &root.join("catalog"))
            .with_context(|| format!("copy {fixture} site catalog"))?;
        // A present site catalog must hold every declaration directory, even empty ones.
        for directory in ["models", "engines", "applications"] {
            fs::create_dir_all(root.join("catalog").join(directory))
                .with_context(|| format!("create {fixture} site catalog {directory}"))?;
        }
    }
    Ok(site.site_id)
}

/// Loads the master catalog with the site catalog under `root`, as the backend does.
pub fn load_catalog(root: &Path) -> anyhow::Result<Catalog> {
    Catalog::load_sources_with_policy(
        &master_catalog_root(),
        &root.join("catalog"),
        OciArtifactPolicy::Development,
        dotcore::model::engine::declaration_validator(),
    )
    .context("load the master and site catalog")
}

/// One observation of the qualification GPU, the first NVIDIA device as the `nvidia.com/gpu=0`
/// selector names it, taken through the backend's hardware collector.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuObservation {
    pub device_id: String,
    pub driver: String,
    pub memory: GpuMemory,
    pub processes: Vec<GpuProcess>,
}

impl GpuObservation {
    /// Collects the host's hardware the way a node does and reads the qualification GPU from it.
    pub fn collect() -> anyhow::Result<Self> {
        let collector = SystemHardwareSnapshotCollector::for_current_host(
            vec![Box::new(SystemNvidiaAcceleratorAdapter::for_current_host())],
            Arc::new(SystemClock),
        );
        Self::from_hardware(&collector.collect_observation())
    }

    /// Reads the first NVIDIA device, its memory pool, and the compute processes on it.
    pub fn from_hardware(observation: &NodeHardwareObservation) -> anyhow::Result<Self> {
        let accelerator = observation
            .snapshot
            .accelerators
            .iter()
            .find(|accelerator| accelerator.vendor == AcceleratorVendor::Nvidia as i32)
            .context("the hardware observation has no NVIDIA result")?;
        let facts = match accelerator.result.as_ref() {
            Some(accelerator_observation::Result::Facts(facts)) => &facts.facts,
            Some(accelerator_observation::Result::Unavailable(unavailable)) => {
                anyhow::bail!("NVIDIA facts are unavailable: {}", unavailable.message)
            }
            None => anyhow::bail!("the NVIDIA observation has no result"),
        };
        let device = facts.first().context("no NVIDIA device is present")?;
        let memory = match AcceleratorMemoryArchitecture::try_from(device.memory_architecture) {
            Ok(AcceleratorMemoryArchitecture::Dedicated) => GpuMemory::Dedicated {
                total_bytes: device
                    .total_memory_bytes
                    .context("the dedicated NVIDIA device reports no total memory")?,
                used_bytes: device
                    .used_memory_bytes
                    .context("the dedicated NVIDIA device reports no used memory")?,
            },
            Ok(AcceleratorMemoryArchitecture::Unified) => {
                let Some(host_observation::Result::Facts(host)) = observation
                    .snapshot
                    .host
                    .as_ref()
                    .and_then(|host| host.result.as_ref())
                else {
                    anyhow::bail!("host memory of the unified NVIDIA device is unavailable");
                };
                GpuMemory::Unified {
                    host_total_bytes: host.total_memory_bytes,
                    host_available_bytes: host.available_memory_bytes,
                }
            }
            architecture => anyhow::bail!(
                "the NVIDIA device memory architecture is not classified: {architecture:?}"
            ),
        };
        let processes = match &observation.accelerator_processes {
            AcceleratorProcessObservation::Observed(vendors) => vendors
                .iter()
                .find(|vendor| vendor.vendor == AcceleratorVendor::Nvidia)
                .context("the process observation has no NVIDIA rows")?,
            AcceleratorProcessObservation::Unavailable(unavailable) => {
                anyhow::bail!("GPU processes are unavailable: {}", unavailable.message)
            }
        };
        let processes = match &processes.result {
            AcceleratorVendorProcessResult::Observed(processes) => processes
                .iter()
                .filter(|process| process.device_id == device.device_id)
                .map(|process| GpuProcess {
                    process_id: process.process_id,
                    ancestor_ids: process
                        .ancestors
                        .iter()
                        .map(|ancestor| ancestor.process_id)
                        .collect(),
                    used_memory_bytes: process.used_memory_bytes,
                })
                .collect(),
            AcceleratorVendorProcessResult::Unavailable(unavailable) => {
                anyhow::bail!("NVIDIA processes are unavailable: {}", unavailable.message)
            }
        };
        Ok(Self {
            device_id: device.device_id.clone(),
            driver: device.driver_or_runtime_version.clone(),
            memory,
            processes,
        })
    }

    /// Checks that the GPU's pool has `required_bytes` free; other workloads may hold the rest.
    pub fn check_free_memory(&self, required_bytes: u64) -> anyhow::Result<()> {
        let free = self.memory.free_bytes();
        anyhow::ensure!(
            free >= required_bytes,
            "NVIDIA qualification requires {required_bytes} free {} bytes; found {free}",
            self.memory.pool()
        );
        Ok(())
    }

    /// Returns the workload rooted at `root_process_id`: the GPU processes that are the root or
    /// descend from it. `None` while none holds the GPU yet.
    pub fn workload(&self, root_process_id: u32) -> anyhow::Result<Option<GpuWorkload>> {
        let processes = self
            .processes
            .iter()
            .filter(|process| {
                process.process_id == root_process_id
                    || process.ancestor_ids.contains(&root_process_id)
            })
            .collect::<Vec<_>>();
        if processes.is_empty() {
            return Ok(None);
        }
        let mut process_memory_bytes = 0;
        for process in &processes {
            process_memory_bytes += process.used_memory_bytes.with_context(|| {
                format!(
                    "NVIDIA reports no memory for workload process {}",
                    process.process_id
                )
            })?;
        }
        anyhow::ensure!(
            process_memory_bytes > 0,
            "NVIDIA reports zero memory for the workload rooted at process {root_process_id}"
        );
        Ok(Some(GpuWorkload {
            device_id: self.device_id.clone(),
            process_ids: processes.iter().map(|process| process.process_id).collect(),
            process_memory_bytes,
        }))
    }

    /// Returns whether `workload` released the GPU: none of its processes remain. Device-wide
    /// memory is not compared, since other workloads may allocate or free it meanwhile.
    pub fn released(&self, workload: &GpuWorkload) -> bool {
        self.processes
            .iter()
            .all(|process| !workload.process_ids.contains(&process.process_id))
    }
}

/// How far host available memory may move between two readings of a unified pool that holds
/// still.
const UNIFIED_STEADY_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// The memory pool the qualification GPU allocates from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuMemory {
    Dedicated {
        total_bytes: u64,
        used_bytes: u64,
    },

    Unified {
        host_total_bytes: u64,
        host_available_bytes: u64,
    },
}

impl GpuMemory {
    /// Returns the bytes a new workload can still allocate from the pool.
    pub fn free_bytes(&self) -> u64 {
        match self {
            Self::Dedicated {
                total_bytes,
                used_bytes,
            } => total_bytes.saturating_sub(*used_bytes),
            Self::Unified {
                host_available_bytes,
                ..
            } => *host_available_bytes,
        }
    }

    /// Returns whether the pool holds still since `previous`: dedicated use exactly, host
    /// available memory within [`UNIFIED_STEADY_MARGIN_BYTES`], since the host's other processes
    /// move it every second.
    pub fn steady_since(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::Unified { .. }, Self::Unified { .. }) => {
                self.free_bytes().abs_diff(previous.free_bytes()) <= UNIFIED_STEADY_MARGIN_BYTES
            }
            _ => self == previous,
        }
    }

    fn pool(&self) -> &'static str {
        match self {
            Self::Dedicated { .. } => "dedicated GPU memory",
            Self::Unified { .. } => "host memory (unified GPU)",
        }
    }
}

impl fmt::Display for GpuMemory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dedicated {
                total_bytes,
                used_bytes,
            } => write!(
                formatter,
                "dedicated used_bytes={used_bytes} total_bytes={total_bytes}"
            ),
            Self::Unified {
                host_total_bytes,
                host_available_bytes,
            } => write!(
                formatter,
                "unified host_available_bytes={host_available_bytes} host_total_bytes={host_total_bytes}"
            ),
        }
    }
}

/// One compute process on the qualification GPU, with its ancestors' process ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuProcess {
    pub process_id: u32,
    pub ancestor_ids: Vec<u32>,
    pub used_memory_bytes: Option<u64>,
}

/// The GPU processes of one running workload and their memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuWorkload {
    pub device_id: String,
    pub process_ids: BTreeSet<u32>,
    pub process_memory_bytes: u64,
}

/// Checks that the GPU has `required_bytes` free in its pool, logging what was read, including the
/// compute processes of other workloads sharing it.
pub fn ensure_free_gpu_memory(suite: &str, required_bytes: u64) -> anyhow::Result<GpuObservation> {
    let observation = GpuObservation::collect().context("observe the qualification GPU")?;
    observation.check_free_memory(required_bytes)?;
    eprintln!(
        "[{suite}] operation=GPU preflight state=available device={} driver={} memory=({}) free_bytes={} other_processes={}",
        observation.device_id,
        observation.driver,
        observation.memory,
        observation.memory.free_bytes(),
        observation.processes.len(),
    );
    Ok(observation)
}

/// Whether the engine is a Docker-delivered Nix artifact of the development source, whose launch
/// identity the hosting node pins when it prepares it.
pub fn development_source_docker_image(engine: &ResolvedEngineArtifact) -> bool {
    let Some(EngineArtifactKind::Nix(nix)) = engine
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.kind.as_ref())
    else {
        return false;
    };
    nix.flake == "self"
        && matches!(nix.provides, Some(Provides::DockerImage(_)))
        && engine.preparation_policy == ArtifactPreparationPolicy::DevelopmentSource as i32
        && engine.launch_identity.is_empty()
}

/// Launch identity the hosting node must pin for a development-source Docker image engine on this
/// host, and the Docker reference its loader tags the image with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnginePin {
    pub store_path: String,
    pub launch_reference: String,
}

impl EnginePin {
    /// Evaluates the engine's package from the development source for this host's Nix system, as
    /// the hosting node does, without building it.
    pub async fn evaluate(
        dotcore_sources: &DotcoreSources,
        engine: &ResolvedEngineArtifact,
    ) -> anyhow::Result<Self> {
        let Some(EngineArtifactKind::Nix(nix)) = engine
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.kind.as_ref())
        else {
            anyhow::bail!("engine {} has no Nix artifact", engine.engine_release_id);
        };
        let package = nix
            .package
            .as_deref()
            .context("engine Nix artifact has no package")?;
        let source = dotcore_sources
            .development
            .as_deref()
            .context("qualification has no development source")?;
        let system = dotcore_domain::nix::host_system().context("this host has no Nix system")?;
        let installable = dotcore_domain::nix::installable(source, system, package);
        let output = tokio::process::Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "eval",
                "--raw",
                "--no-write-lock-file",
                &format!("{installable}.outPath"),
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .context("run nix to evaluate the engine package")?;
        anyhow::ensure!(
            output.status.success(),
            "evaluate {installable}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let store_path = String::from_utf8(output.stdout).context("read engine store path")?;
        let launch_reference = dotcore_domain::nix::docker_launch_reference(&store_path)
            .with_context(|| format!("{store_path} is not a Docker image derivation"))?;
        Ok(Self {
            store_path,
            launch_reference,
        })
    }

    /// Checks that the deployment recorded exactly this store path as its engine pin.
    pub fn check(&self, deployment: &ModelDeployment) -> anyhow::Result<()> {
        let expected = [NodeResolvedLaunchIdentity {
            selection_key: ENGINE_SELECTION_KEY.to_owned(),
            launch_identity: self.store_path.clone(),
        }];
        anyhow::ensure!(
            deployment.node_resolved_launch_identities == expected,
            "deployment {} engine pin differs from {}: {:?}",
            deployment.model_deployment_id,
            self.store_path,
            deployment.node_resolved_launch_identities
        );
        Ok(())
    }

    /// Returns whether the local Docker daemon already holds the loader's image.
    pub async fn image_present(&self) -> anyhow::Result<bool> {
        let docker = Docker::connect_with_local_defaults().context("connect Docker")?;
        match docker.inspect_image(&self.launch_reference).await {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(error) => Err(anyhow::Error::new(error)
                .context(format!("inspect engine image {}", self.launch_reference))),
        }
    }
}

/// Returns whether the hosting node finished preparing the deployment's engine: it recorded the
/// engine pin and created the deployment's Docker workload, which it launches only after the
/// loader tagged and verified the engine image. The deployment status cannot tell this apart, as
/// it stays Loading from creation through preparation and startup.
pub async fn engine_prepared(
    configuration: &SitePaths,
    deployment: &ModelDeployment,
) -> anyhow::Result<bool> {
    if deployment.node_resolved_launch_identities.is_empty() {
        return Ok(false);
    }
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))
        .context("read qualification site")?;
    let filters = HashMap::from([(
        "label",
        vec![
            format!("{LABEL_CORRELATION}={}", deployment.model_deployment_id),
            format!("{LABEL_SITE}={}", site.site_id),
            format!("{LABEL_SCHEMA}={OWNERSHIP_SCHEMA}"),
        ],
    )]);
    let containers = Docker::connect_with_local_defaults()
        .context("connect Docker")?
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .context("list deployment workloads")?;
    Ok(!containers.is_empty())
}

/// Waits until the workload rooted at `root_process_id` holds GPU memory.
pub async fn wait_for_workload(
    root_process_id: u32,
    timeout: Duration,
) -> anyhow::Result<GpuWorkload> {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(workload) = GpuObservation::collect()?.workload(root_process_id)? {
                return Ok(workload);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| format!("no GPU process of workload root {root_process_id} appeared"))?
}

/// Waits until `workload` released the GPU, returning the observation that proved it.
pub async fn wait_for_release(
    workload: &GpuWorkload,
    timeout: Duration,
) -> anyhow::Result<GpuObservation> {
    let mut last = None;
    let released = tokio::time::timeout(timeout, async {
        loop {
            let observation = GpuObservation::collect()?;
            if observation.released(workload) {
                return Ok::<_, anyhow::Error>(observation);
            }
            last = Some(observation);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    match released {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "workload {workload:?} did not release the GPU; last observation: {last:?}"
        )),
    }
}

/// One profile to launch at the largest fit the capacity component computes for the idle GPU.
pub struct LargestFitCase<'a> {
    pub suite: &'a str,
    pub model_id: &'a str,
    pub profile_id: &'a str,
    pub node_id: &'a str,
    pub target_id: &'a str,
}

/// Deploys the case's profile at the starting total context the compatibility read gives on the
/// idle GPU (its largest fit), or a smaller one on a unified GPU so the host keeps memory, waits
/// for Ready, and compares the memory held after load with the capacity component's estimate for
/// that context: GPU memory on a dedicated GPU, host memory on a unified one. Removes the
/// deployment and waits for the GPU to be released before returning. Skips, saying why, when the
/// profile's engine has no artifact for this host's platform.
pub async fn qualify_largest_fit(
    site: &QualificationSite,
    case: &LargestFitCase<'_>,
) -> anyhow::Result<()> {
    if let Some(reason) = site.unsupported_platform_reason(case.model_id, case.profile_id)? {
        eprintln!("[{}] skipped: {reason}", case.suite);
        return Ok(());
    }
    let idle = ensure_free_gpu_memory(case.suite, 0)?;
    anyhow::ensure!(
        idle.processes.is_empty(),
        "the largest fit is judged on an idle GPU; processes hold it: {:?}",
        idle.processes
    );
    let backend = crate::support::RunningBackend::start_at(
        site.configuration.clone(),
        "127.0.0.1".parse().expect("loopback bind address"),
        site.dotcore_sources.clone(),
    )
    .await;
    let result = load_at_largest_fit(&backend, site, case, &idle).await;
    backend.stop().await;
    result
}

async fn load_at_largest_fit(
    backend: &crate::support::RunningBackend,
    site: &QualificationSite,
    case: &LargestFitCase<'_>,
    idle: &GpuObservation,
) -> anyhow::Result<()> {
    let suite = case.suite;
    let mut service = backend.model_client();
    let placement = crate::support::exact_placement(case.node_id, case.target_id);
    let (largest, starting) = wait_for_largest_fit(&mut service, case, &placement).await?;
    let (total_context, configuration, estimate, demand) = host_safe_load(
        &mut service,
        &site.catalog,
        case,
        &placement,
        &idle.memory,
        starting,
    )
    .await?;
    eprintln!(
        "[{suite}] model={} profile={} largest_fit={largest} starting_total_context={starting} \
         total_context={total_context} estimate_gpu_bytes={demand} idle={} estimates={:?} \
         derived={:?}",
        case.model_id,
        case.profile_id,
        idle.memory,
        estimate.hardware_class_estimates,
        estimate
            .hardware_class_estimates
            .iter()
            .map(|class| &class.derived_engine_parameter_values)
            .collect::<Vec<_>>(),
    );

    let started = Instant::now();
    let created = service
        .create_model_deployment(tonic::Request::new(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: case.model_id.to_owned(),
                    profile_id: case.profile_id.to_owned(),
                    placement: Some(placement),
                    configuration: Some(configuration),
                },
            )),
            exposure: Some(crate::support::hidden_model_exposure()),
            model_deployment_name: None,
        }))
        .await
        .context("create the largest-fit deployment")?
        .into_inner()
        .deployment
        .context("creation returned no deployment")?;
    let deployment_id = created.model_deployment_id;
    let loaded = wait_until_ready(&mut service, suite, &deployment_id).await;
    let measured = match &loaded {
        Ok(()) => Some(settled_gpu_after_load(&mut service, &deployment_id).await),
        Err(_) => None,
    };
    let ready_after = started.elapsed();

    service
        .remove_model_deployment(tonic::Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        }))
        .await
        .context("remove the largest-fit deployment")?;
    let released = tokio::time::timeout(RELEASE_TIMEOUT, async {
        while !GpuObservation::collect()?.processes.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        GpuObservation::collect()
    })
    .await
    .context("the removed deployment kept the GPU")??;
    eprintln!("[{suite}] released: {}", released.memory);
    loaded?;

    let (after, readings) = measured.expect("measured after a ready load")?;
    // On a unified GPU the load draws from host memory, so what it holds is the drop in host
    // available memory.
    let free_before = idle.memory.free_bytes();
    let held = free_before.saturating_sub(after.memory.free_bytes());
    let process_bytes: u64 = after
        .processes
        .iter()
        .filter_map(|process| process.used_memory_bytes)
        .sum();
    eprintln!(
        "[{suite}] ready_after_ms={} estimate={demand} after=({}) held={held} \
         process_bytes={process_bytes} deployment_reading={readings:?} \
         estimate_minus_held_negative_is_over={}",
        ready_after.as_millis(),
        after.memory,
        i128::from(demand) - i128::from(held),
    );
    // The estimate fills the free memory less a headroom, so a load may pass it by less than the
    // headroom but must stay within the memory that was free.
    anyhow::ensure!(
        held <= free_before,
        "[{suite}] the load held {held} B on the GPU, {} B over the {free_before} B that were free",
        held - free_before
    );
    Ok(())
}

/// Picks the total context the test loads, returning it with its configuration, estimate and
/// previewed demand. A dedicated GPU loads the largest fit. A unified GPU shares its memory with
/// the OS, so a load filling it can freeze the host: the context halves from the largest fit until
/// the estimate leaves the host reserve of the free host memory.
pub async fn host_safe_load(
    service: &mut ModelClient,
    catalog: &Catalog,
    case: &LargestFitCase<'_>,
    placement: &ExactPlacement,
    idle: &GpuMemory,
    largest_fit: u32,
) -> anyhow::Result<(
    u32,
    LocalModelDeploymentConfiguration,
    EstimateLocalModelResourcesResponse,
    u64,
)> {
    let reserve = match idle {
        GpuMemory::Dedicated { .. } => None,
        GpuMemory::Unified {
            host_total_bytes, ..
        } => Some(host_reserve_bytes(*host_total_bytes)),
    };
    let budget = reserve.map(|reserve| idle.free_bytes().saturating_sub(reserve));
    let mut total_context = largest_fit;
    loop {
        let configuration = configuration_at(catalog, case, total_context)?;
        let estimate = service
            .estimate_local_model_resources(tonic::Request::new(
                EstimateLocalModelResourcesRequest {
                    model_id: case.model_id.to_owned(),
                    profile_id: case.profile_id.to_owned(),
                    configuration: Some(configuration.clone()),
                    placement: Some(placement.clone()),
                },
            ))
            .await
            .with_context(|| format!("estimate total context {total_context}"))?
            .into_inner();
        let demand = previewed_device_demand(&estimate)?;
        match budget {
            Some(budget) if demand > budget => {
                anyhow::ensure!(
                    total_context > 1,
                    "the model needs {demand} B, over the {budget} B a unified GPU may load \
                     while leaving {} B to the host",
                    reserve.unwrap_or_default()
                );
                total_context /= 2;
            }
            _ => return Ok((total_context, configuration, estimate, demand)),
        }
    }
}

/// Waits for the compatibility read to give the profile's largest fit on the target, returning it
/// with the starting total context a deployment begins from.
pub async fn wait_for_largest_fit(
    service: &mut ModelClient,
    case: &LargestFitCase<'_>,
    placement: &ExactPlacement,
) -> anyhow::Result<(u32, u32)> {
    let mut last = None;
    let found = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let response = service
                .list_model_profile_compatibility(tonic::Request::new(
                    ListModelProfileCompatibilityRequest {
                        model_id: Some(case.model_id.to_owned()),
                        target: Some(placement.clone()),
                        scenario: None,
                    },
                ))
                .await?
                .into_inner();
            let entry = response
                .entries
                .into_iter()
                .find(|entry| entry.profile_id == case.profile_id);
            if let Some(entry) = &entry
                && let (Some(largest), Some(starting)) = (
                    entry.largest_total_context_tokens,
                    entry.starting_total_context_tokens,
                )
            {
                anyhow::ensure!(
                    entry.starting_total_context_source
                        == StartingTotalContextSource::LargestFit as i32,
                    "the starting total context is not the largest fit: {entry:?}"
                );
                return Ok::<_, anyhow::Error>((largest, starting));
            }
            last = entry;
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    match found {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "no largest fit on the idle GPU; last entry: {last:?}"
        )),
    }
}

/// The profile's defaults with the total context set.
fn configuration_at(
    catalog: &Catalog,
    case: &LargestFitCase<'_>,
    total_context: u32,
) -> anyhow::Result<LocalModelDeploymentConfiguration> {
    let records = catalog.protobuf_records_without_sizing();
    let profile = records
        .models
        .iter()
        .find(|model| model.model_id == case.model_id)
        .and_then(|model| {
            model
                .profiles
                .iter()
                .find(|profile| profile.profile_id == case.profile_id)
        })
        .with_context(|| format!("no profile {} of {}", case.profile_id, case.model_id))?;
    let mut values = profile
        .engine_parameter_defaults
        .clone()
        .unwrap_or_default();
    values
        .values
        .retain(|value| value.parameter_id != "total-context");
    values.values.push(EngineParameterValue {
        parameter_id: "total-context".to_owned(),
        value: Some(engine_parameter_value::Value::UnsignedInteger(u64::from(
            total_context,
        ))),
    });
    Ok(LocalModelDeploymentConfiguration {
        model_artifact_binding_id: profile.default_model_artifact_binding_id.clone(),
        engine_parameter_values: Some(values),
        custom_argument_tokens: Vec::new(),
        option_overrides: Vec::new(),
    })
}

/// The previewed demand on the pool the model's GPU allocates from: its device pool, or the host
/// pool for a unified GPU, which has none.
fn previewed_device_demand(estimate: &EstimateLocalModelResourcesResponse) -> anyhow::Result<u64> {
    let preview = estimate
        .placement_preview
        .as_ref()
        .context("the estimate has no placement preview")?;
    let pools = || preview.pools.iter().filter_map(|pool| pool.pool.as_ref());
    let pool = pools()
        .find(|pool| pool.kind == MemoryPoolKind::Device as i32)
        .or_else(|| pools().find(|pool| pool.kind == MemoryPoolKind::Host as i32))
        .context("the placement preview has no device or host pool")?;
    Ok(pool
        .segments
        .iter()
        .filter(|segment| segment.source == MemorySegmentSource::Previewed as i32)
        .map(|segment| segment.bytes)
        .sum())
}

async fn wait_until_ready(
    service: &mut ModelClient,
    suite: &str,
    deployment_id: &str,
) -> anyhow::Result<()> {
    let mut last_status = None;
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            let deployment = service
                .list_model_deployments(tonic::Request::new(ListModelDeploymentsRequest {}))
                .await?
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
                .context("the deployment disappeared")?;
            let state = deployment.state.context("the deployment has no state")?;
            let status = ModelDeploymentStatus::try_from(state.status);
            if last_status != Some(state.status) {
                eprintln!("[{suite}] status={status:?}");
                last_status = Some(state.status);
            }
            match status {
                Ok(ModelDeploymentStatus::Ready) => return Ok(()),
                Ok(
                    ModelDeploymentStatus::Unspecified
                    | ModelDeploymentStatus::Queued
                    | ModelDeploymentStatus::Preparing
                    | ModelDeploymentStatus::Downloading
                    | ModelDeploymentStatus::Loading
                    | ModelDeploymentStatus::WarmingUp,
                ) => {}
                other => anyhow::bail!("left startup as {other:?}: {:?}", state.failure),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("the deployment never reached Ready")?
}

/// Reads the GPU once its memory held steady across consecutive readings, with the deployment's
/// own attributed reading.
async fn settled_gpu_after_load(
    service: &mut ModelClient,
    deployment_id: &str,
) -> anyhow::Result<(GpuObservation, Option<u64>)> {
    let mut previous = GpuObservation::collect()?;
    let mut steady = 0;
    let mut waited = Duration::ZERO;
    while steady < 5 && waited < Duration::from_secs(120) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        waited += Duration::from_secs(1);
        let current = GpuObservation::collect()?;
        steady = if current.memory.steady_since(&previous.memory) {
            steady + 1
        } else {
            0
        };
        previous = current;
    }
    let deployment = service
        .list_model_deployments(tonic::Request::new(ListModelDeploymentsRequest {}))
        .await?
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .context("the deployment disappeared")?;
    let reading = match deployment
        .accelerator_memory
        .and_then(|memory| memory.result)
    {
        Some(accelerator_memory_observation::Result::Available(usage)) => Some(
            usage
                .devices
                .iter()
                .map(|device| device.used_memory_bytes)
                .sum(),
        ),
        _ => None,
    };
    Ok((previous, reading))
}

/// Reads the execution platform of the local Docker daemon, which runs the qualification workloads.
async fn native_runtime_platform() -> anyhow::Result<ExecutionPlatform> {
    let info = Docker::connect_with_local_defaults()
        .context("connect native Docker runtime")?
        .info()
        .await
        .context("read native Docker runtime platform")?;
    let platform = execution_platform_from_docker_info(&info)
        .context("normalize native Docker runtime platform")?;
    eprintln!("[nvidia-qualification] native platform={platform}");
    Ok(platform)
}

/// Normalizes the Docker daemon's operating system type, not its distribution name.
fn execution_platform_from_docker_info(
    information: &SystemInfo,
) -> Result<ExecutionPlatform, ExecutionPlatformError> {
    ExecutionPlatform::normalize(
        information.os_type.as_deref().unwrap_or_default(),
        information.architecture.as_deref().unwrap_or_default(),
    )
}

/// Returns the skip reason for a profile whose engine release approves no NVIDIA artifact for
/// `platform`.
fn unsupported_platform_reason(
    catalog: &Catalog,
    model_id: &str,
    profile_id: &str,
    platform: &ExecutionPlatform,
) -> anyhow::Result<Option<String>> {
    let records = catalog.protobuf_records_without_sizing();
    let binding_id = records
        .models
        .iter()
        .find(|model| model.model_id == model_id)
        .and_then(|model| {
            model
                .profiles
                .iter()
                .find(|profile| profile.profile_id == profile_id)
        })
        .map(|profile| profile.default_model_artifact_binding_id.clone())
        .with_context(|| format!("Catalog has no profile {model_id}/{profile_id}"))?;
    let declarations = catalog
        .local_model_declarations(
            &CatalogModelId(model_id.to_owned()),
            profile_id,
            &binding_id,
        )
        .with_context(|| format!("read catalog declarations of {model_id}/{profile_id}"))?;
    let artifacts = declarations
        .approved_engine_artifacts
        .iter()
        .map(|approved| &approved.artifact);
    Ok((!nvidia_artifact_supports(artifacts, platform)).then(|| {
        format!(
            "configuration {} approves no NVIDIA artifact for {platform}",
            declarations.configuration.configuration_id
        )
    }))
}

fn nvidia_artifact_supports<'a>(
    artifacts: impl IntoIterator<Item = &'a EngineArtifact>,
    platform: &ExecutionPlatform,
) -> bool {
    artifacts
        .into_iter()
        .flat_map(|artifact| &artifact.selectors)
        .any(|supported| {
            supported
                .accelerators
                .iter()
                .any(|vendor| vendor == NVIDIA_ACCELERATOR)
                && supported
                    .platform
                    .as_ref()
                    .and_then(|declared| ExecutionPlatform::try_from(declared).ok())
                    .as_ref()
                    == Some(platform)
        })
}

fn acquire_lock() -> anyhow::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(crate::support::canonical_temp_dir().join(LOCK_FILE))
        .context("open the host-local NVIDIA qualification lock")?;
    anyhow::ensure!(
        lock.try_lock_exclusive()
            .context("acquire the host-local NVIDIA qualification lock")?,
        "another NVIDIA qualification is already running"
    );
    Ok(lock)
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn master_catalog_root() -> PathBuf {
    crate::support::workspace_root().join("catalog")
}

/// Returns the Dotcore sources of a qualification run: the workspace development source, which
/// the hosting node builds the catalog's development-source engines from.
fn development_sources() -> anyhow::Result<DotcoreSources> {
    development_sources_from(std::env::var(DEV_SOURCE_FLAKE_ENV).ok())
}

fn development_sources_from(value: Option<String>) -> anyhow::Result<DotcoreSources> {
    let development = value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .with_context(|| {
            format!(
                "{DEV_SOURCE_FLAKE_ENV} is required for NVIDIA qualification: the hosting node builds \
                 the catalog's development-source engines from it; run the suite in the workspace \
                 environment, which sets it to the workspace flake"
            )
        })?;
    Ok(DotcoreSources {
        development: Some(development),
        packaged: None,
    })
}

fn shared_cache_root() -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var("DOTCORE_CACHE_DIR")
            .context("DOTCORE_CACHE_DIR is required for NVIDIA qualification")?,
    );
    anyhow::ensure!(
        path.is_absolute() && path != Path::new("/"),
        "the shared cache root must be an absolute non-root path"
    );
    Ok(path)
}

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use dotcore_domain::hardware_observation::{
        AcceleratorVendorProcessObservation, ObservedAcceleratorProcess,
    };
    use dotcore_host_process::ProcessLifetime;
    use dotcore_proto::dotcore::{
        catalog::v1::Selector,
        observation::v1::{
            AcceleratorFacts, AcceleratorFactsSet, AcceleratorObservation, HardwareSnapshot,
            HostFacts, HostObservation, ObservationUnavailable,
        },
    };

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn dedicated_memory_frees_the_unused_device_bytes() {
        let observation =
            GpuObservation::from_hardware(&hardware(dedicated(32 * GIB, 2 * GIB), &[]))
                .expect("dedicated observation");

        assert_eq!(observation.device_id, "GPU-0");
        assert_eq!(observation.driver, "580.95.05");
        assert_eq!(
            observation.memory,
            GpuMemory::Dedicated {
                total_bytes: 32 * GIB,
                used_bytes: 2 * GIB,
            }
        );
        assert_eq!(observation.memory.free_bytes(), 30 * GIB);
    }

    #[test]
    fn unified_memory_frees_the_available_host_bytes() {
        let observation =
            GpuObservation::from_hardware(&hardware(unified(), &[])).expect("unified observation");

        assert_eq!(
            observation.memory,
            GpuMemory::Unified {
                host_total_bytes: 128 * GIB,
                host_available_bytes: 100 * GIB,
            }
        );
        assert_eq!(observation.memory.free_bytes(), 100 * GIB);
    }

    #[test]
    fn an_unclassified_device_memory_is_refused() {
        let mut device = dedicated(32 * GIB, 2 * GIB);
        device.memory_architecture = AcceleratorMemoryArchitecture::Unspecified as i32;

        let error =
            GpuObservation::from_hardware(&hardware(device, &[])).expect_err("unclassified memory");

        assert_eq!(
            error.to_string(),
            "the NVIDIA device memory architecture is not classified: Ok(Unspecified)"
        );
    }

    #[test]
    fn unavailable_processes_are_refused_rather_than_read_as_idle() {
        let mut observation = hardware(dedicated(32 * GIB, 2 * GIB), &[]);
        observation.accelerator_processes =
            AcceleratorProcessObservation::Unavailable(ObservationUnavailable {
                code: "PROCESS_OBSERVATION_UNAVAILABLE".to_owned(),
                message: "unreadable".to_owned(),
            });

        let error = GpuObservation::from_hardware(&observation).expect_err("no process rows");

        assert_eq!(
            error.to_string(),
            "GPU processes are unavailable: unreadable"
        );
    }

    #[test]
    fn preflight_shares_the_gpu_with_other_processes_when_enough_memory_is_free() {
        let observation = GpuObservation::from_hardware(&hardware(
            dedicated(32 * GIB, 15 * GIB),
            &[
                process("GPU-0", 41, &[1], Some(4 * GIB)),
                process("GPU-0", 42, &[1], Some(7 * GIB)),
            ],
        ))
        .expect("shared observation");

        assert_eq!(observation.memory.free_bytes(), 17 * GIB);
        observation
            .check_free_memory(17 * GIB)
            .expect("other workloads leave enough memory");
        assert_eq!(
            observation
                .check_free_memory(18 * GIB)
                .expect_err("other workloads leave too little memory")
                .to_string(),
            format!(
                "NVIDIA qualification requires {} free dedicated GPU memory bytes; found {}",
                18 * GIB,
                17 * GIB
            )
        );
    }

    #[test]
    fn preflight_names_the_pool_without_enough_free_memory() {
        let dedicated = GpuObservation::from_hardware(&hardware(dedicated(8 * GIB, 2 * GIB), &[]))
            .expect("dedicated observation");
        let unified =
            GpuObservation::from_hardware(&hardware(unified(), &[])).expect("unified observation");

        assert_eq!(
            dedicated
                .check_free_memory(10 * GIB)
                .expect_err("small device")
                .to_string(),
            format!(
                "NVIDIA qualification requires {} free dedicated GPU memory bytes; found {}",
                10 * GIB,
                6 * GIB
            )
        );
        assert_eq!(
            unified
                .check_free_memory(128 * GIB)
                .expect_err("small host")
                .to_string(),
            format!(
                "NVIDIA qualification requires {} free host memory (unified GPU) bytes; found {}",
                128 * GIB,
                100 * GIB
            )
        );
        dedicated
            .check_free_memory(6 * GIB)
            .expect("enough device memory");
        unified
            .check_free_memory(100 * GIB)
            .expect("enough host memory");
    }

    #[test]
    fn the_workload_is_its_root_and_descendants_on_the_device() {
        let observation = GpuObservation::from_hardware(&hardware(
            dedicated(32 * GIB, 12 * GIB),
            &[
                process("GPU-0", 200, &[100, 1], Some(6 * GIB)),
                process("GPU-0", 201, &[200, 100, 1], Some(2 * GIB)),
                process("GPU-0", 300, &[1], Some(GIB)),
                process("GPU-1", 202, &[100, 1], Some(GIB)),
            ],
        ))
        .expect("observation");

        assert_eq!(
            observation.workload(100).expect("workload"),
            Some(GpuWorkload {
                device_id: "GPU-0".to_owned(),
                process_ids: BTreeSet::from([200, 201]),
                process_memory_bytes: 8 * GIB,
            })
        );
        assert_eq!(observation.workload(999).expect("no workload"), None);
    }

    #[test]
    fn a_workload_process_without_memory_is_refused() {
        let observation = GpuObservation::from_hardware(&hardware(
            unified(),
            &[process("GPU-0", 100, &[1], None)],
        ))
        .expect("observation");

        assert_eq!(
            observation
                .workload(100)
                .expect_err("no memory")
                .to_string(),
            "NVIDIA reports no memory for workload process 100"
        );
    }

    #[test]
    fn release_needs_only_the_workload_processes_gone() {
        let workload = GpuWorkload {
            device_id: "GPU-0".to_owned(),
            process_ids: BTreeSet::from([200, 201]),
            process_memory_bytes: 8 * GIB,
        };
        let child_remains = GpuObservation::from_hardware(&hardware(
            dedicated(32 * GIB, 4 * GIB),
            &[process("GPU-0", 201, &[200, 100, 1], Some(2 * GIB))],
        ))
        .expect("child remains");
        // Another workload grew while this one stopped, so device memory rose instead of falling.
        let others_grew = GpuObservation::from_hardware(&hardware(
            dedicated(32 * GIB, 20 * GIB),
            &[
                process("GPU-0", 300, &[1], Some(6 * GIB)),
                process("GPU-0", 301, &[1], Some(14 * GIB)),
            ],
        ))
        .expect("other workloads grew");
        let unified_others = GpuObservation::from_hardware(&hardware(
            unified(),
            &[process("GPU-0", 300, &[1], Some(GIB))],
        ))
        .expect("unified other process");

        assert!(!child_remains.released(&workload));
        assert!(others_grew.released(&workload));
        assert!(unified_others.released(&workload));
    }

    #[test]
    fn a_profile_skips_when_no_nvidia_artifact_matches_the_platform() {
        let amd64_only = [artifact(&[("linux", "amd64", "nvidia")])];
        let arm64_without_nvidia = [artifact(&[
            ("linux", "amd64", "nvidia"),
            ("linux", "arm64", "none"),
        ])];
        let both = [
            artifact(&[("linux", "amd64", "nvidia")]),
            artifact(&[("linux", "arm64", "nvidia")]),
        ];

        assert!(nvidia_artifact_supports(
            &amd64_only,
            &ExecutionPlatform::linux_amd64()
        ));
        assert!(!nvidia_artifact_supports(
            &amd64_only,
            &ExecutionPlatform::linux_arm64()
        ));
        assert!(!nvidia_artifact_supports(
            &arm64_without_nvidia,
            &ExecutionPlatform::linux_arm64()
        ));
        assert!(nvidia_artifact_supports(
            &both,
            &ExecutionPlatform::linux_arm64()
        ));
    }

    #[test]
    fn development_sources_require_a_non_blank_development_source() {
        let expected = "DOTCORE_DEV_SOURCE_FLAKE is required for NVIDIA qualification: the hosting \
                        node builds the catalog's development-source engines from it; run the \
                        suite in the workspace environment, which sets it to the workspace flake";

        for value in [None, Some(" \n".to_owned())] {
            assert_eq!(
                development_sources_from(value)
                    .expect_err("missing development source")
                    .to_string(),
                expected
            );
        }
        assert_eq!(
            development_sources_from(Some(" git+file:///src/dotcore\n".to_owned()))
                .expect("development source"),
            DotcoreSources {
                development: Some("git+file:///src/dotcore".to_owned()),
                packaged: None,
            }
        );
    }

    #[test]
    fn docker_platform_uses_the_os_type_instead_of_the_distribution_name() {
        let information = SystemInfo {
            os_type: Some("linux".to_owned()),
            operating_system: Some("Ubuntu 24.04".to_owned()),
            architecture: Some("aarch64".to_owned()),
            ..Default::default()
        };

        assert_eq!(
            execution_platform_from_docker_info(&information).expect("supported Docker platform"),
            ExecutionPlatform::linux_arm64()
        );
    }

    #[test]
    fn a_materialized_site_targets_the_given_platform() {
        let root = crate::support::canonical_tempdir().expect("temporary site");

        let site_id = materialize_site(
            "vllm-nvidia-site",
            &ExecutionPlatform::linux_arm64(),
            root.path(),
        )
        .expect("materialize vLLM site");

        let site: Site =
            dotcore_proto::yaml::from_file(&root.path().join("site.yaml")).expect("read site");
        assert_eq!(site_id, "vllm-nvidia-docker-integration");
        assert_eq!(
            site.execution_targets
                .iter()
                .map(|target| target.platform.clone())
                .collect::<Vec<_>>(),
            [Some(ExecutionPlatform::linux_arm64().to_oci_platform())]
        );
        assert_eq!(
            site.hardware_observation
                .and_then(|configuration| configuration.refresh_interval_seconds),
            Some(1)
        );
        assert!(
            root.path()
                .join("catalog/models/qwen35-08b-qualification.yaml")
                .is_file()
        );
    }

    fn hardware(
        device: AcceleratorFacts,
        processes: &[ObservedAcceleratorProcess],
    ) -> NodeHardwareObservation {
        NodeHardwareObservation {
            snapshot: HardwareSnapshot {
                observed_at: None,
                host: Some(HostObservation {
                    result: Some(host_observation::Result::Facts(HostFacts {
                        total_memory_bytes: 128 * GIB,
                        available_memory_bytes: 100 * GIB,
                        ..Default::default()
                    })),
                }),
                accelerators: vec![AcceleratorObservation {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    result: Some(accelerator_observation::Result::Facts(
                        AcceleratorFactsSet {
                            facts: vec![
                                device,
                                AcceleratorFacts {
                                    device_id: "GPU-1".to_owned(),
                                    ..dedicated(32 * GIB, 0)
                                },
                            ],
                        },
                    )),
                }],
            },
            accelerator_processes: AcceleratorProcessObservation::Observed(vec![
                AcceleratorVendorProcessObservation {
                    vendor: AcceleratorVendor::Nvidia,
                    result: AcceleratorVendorProcessResult::Observed(processes.to_vec()),
                },
            ]),
        }
    }

    fn dedicated(total_bytes: u64, used_bytes: u64) -> AcceleratorFacts {
        AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
            total_memory_bytes: Some(total_bytes),
            used_memory_bytes: Some(used_bytes),
            driver_or_runtime_version: "580.95.05".to_owned(),
            device_id: "GPU-0".to_owned(),
            ..Default::default()
        }
    }

    fn unified() -> AcceleratorFacts {
        AcceleratorFacts {
            memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
            total_memory_bytes: None,
            used_memory_bytes: None,
            ..dedicated(0, 0)
        }
    }

    fn process(
        device_id: &str,
        process_id: u32,
        ancestor_ids: &[u32],
        used_memory_bytes: Option<u64>,
    ) -> ObservedAcceleratorProcess {
        ObservedAcceleratorProcess {
            device_id: device_id.to_owned(),
            process_id,
            process_start_token: 1,
            ancestors: ancestor_ids
                .iter()
                .map(|&process_id| ProcessLifetime {
                    process_id,
                    start_token: 1,
                })
                .collect(),
            used_memory_bytes,
        }
    }

    fn artifact(platforms: &[(&str, &str, &str)]) -> EngineArtifact {
        EngineArtifact {
            selectors: platforms
                .iter()
                .map(|(operating_system, architecture, accelerator)| Selector {
                    platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                        operating_system: (*operating_system).to_owned(),
                        architecture: (*architecture).to_owned(),
                        variant: None,
                    }),
                    accelerators: vec![(*accelerator).to_owned()],
                    minimum_nvidia_driver_major_version: 580,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
}
