//! Initial site and node configuration for a single-node site.
//!
//! Given an initial request, the new site's identities and the probe report of probed capabilities,
//! bundles the validated site with its node configuration, ready for the caller to write so the new
//! site can start. The same target choice also gives the targets a machine description would
//! declare, for planning a machine that has no site.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use dotcore_domain::{
    ExecutionPlatform, ExecutionPlatformError, names_every_cdi_device, nvidia_cdi_gpu_index,
    nvidia_cdi_gpu_selector, nvidia_cdi_legacy_gpu_selector, selected_accelerator_facts,
};
use dotcore_node_hardware::{MachineDescription, MachineError};
use dotcore_proto::{
    dotcore::{
        node::v1::{DeclaredMachine, LoggingConfig, NodeConfiguration},
        observation::v1::AcceleratorFacts,
        site::v1::{
            ApplicationRuntimeBinding, DockerAcceleratorTransport, DockerApplicationRuntimeAdapter,
            DockerRuntimeAdapter, ExecutionTarget, ModelDeploymentIdentityAllocator,
            ModelRuntimeBinding, NativeRuntimeAdapter, NativeWorkloadLifetime,
            SimulatedApplicationRuntimeAdapter, SimulatedRuntimeAdapter, SimulatedRuntimeScenario,
            Site, SiteManagerRole, SiteNode, application_runtime_binding, execution_target,
            model_runtime_binding,
        },
    },
    yaml,
};

use crate::{
    initial::{
        probe::InitialSiteProbeReport,
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentities,
            InitialSiteRequest, derive_application_execution_target_id,
            derive_model_execution_target_id,
        },
        target_name::{application_target_display_name, model_target_display_name},
    },
    node::binding::{NodeBindingError, bound_site_node},
    repository::SiteRepositoryError,
    validation::{SiteDeclarationRules, load_validated_site, validate_site},
};

/// Time a simulated workload of a new site's default scenario takes to become ready.
/// Not a behavior duration: a value the site declares in its simulated runtime scenario.
const SIMULATED_READY_DELAY_MILLIS: u64 = 2_000;

/// Time a simulated workload of a new site's default scenario takes to stop.
/// Not a behavior duration: a value the site declares in its simulated runtime scenario.
const SIMULATED_STOP_DELAY_MILLIS: u64 = 2_000;

/// Complete in-memory site inputs authored by initial configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct InitialSiteInputs {
    site: Site,
    node: NodeConfiguration,
}

/// Rules for a freshly composed site, which declares no model deployment, so no owner rule is
/// ever reached.
const INITIAL_SITE_RULES: SiteDeclarationRules = SiteDeclarationRules {
    validate_local_model_declaration: |_| {
        Err(dotcore_catalog::CatalogError::Invalid(anyhow::anyhow!(
            "initial site inputs declare no model deployment"
        )))
    },
};

impl InitialSiteInputs {
    /// Composes validated single-node site and local node-binding inputs from probed capabilities.
    pub fn compose(
        request: &InitialSiteRequest,
        identities: InitialSiteIdentities,
        probe: &InitialSiteProbeReport,
    ) -> Result<Self, InitialSiteCompositionError> {
        Self::compose_for_architecture(request, identities, probe, std::env::consts::ARCH)
    }

    /// Composes the inputs with simulated applications placed on the given host CPU architecture.
    fn compose_for_architecture(
        request: &InitialSiteRequest,
        identities: InitialSiteIdentities,
        probe: &InitialSiteProbeReport,
        host_architecture: &str,
    ) -> Result<Self, InitialSiteCompositionError> {
        let choices = NodeTargetChoices::resolve(request, probe, host_architecture)?;
        let execution_targets = build_execution_targets(identities.node_id(), &choices);
        let site = Site {
            site_id: identities.site_id().to_owned(),
            nodes: vec![SiteNode {
                node_id: identities.node_id().to_owned(),
                host: request.site_node_host().to_string(),
                display_name: identities.node_display_name().to_owned(),
                site_manager: Some(SiteManagerRole {}),
                manager_host: String::new(),
            }],
            execution_targets,
            model_deployments: Vec::new(),
            application_deployments: Vec::new(),
            inference_routes: Vec::new(),
            hardware_observation: None,
            model_deployment_identity_allocator: Some(ModelDeploymentIdentityAllocator {
                namespace: crate::store::model_deployment_identity_namespace(identities.site_id()),
                next_sequence: 1,
            }),
        };
        validate_site(&site, &INITIAL_SITE_RULES)
            .map_err(InitialSiteCompositionError::InvalidSite)?;
        Ok(Self {
            node: NodeConfiguration {
                node_id: identities.node_id().to_owned(),
                logging: Some(LoggingConfig {
                    retention_days: crate::node::binding::DEFAULT_LOG_RETENTION_DAYS,
                }),
                declared_machine: choices.declared_machine,
            },
            site,
        })
    }

    /// Loads a validated site and node binding without opening site store or runtime capabilities.
    /// A site that has run may hold deployments and routes alongside what it was initialized with.
    pub fn load(
        site_root: &Path,
        rules: &SiteDeclarationRules,
    ) -> Result<Self, InitialSiteInputLoadError> {
        let site = load_validated_site(&site_root.join(crate::SITE_FILE), rules)?;
        let node = yaml::from_file(&site_root.join(crate::NODE_FILE))?;
        bound_site_node(&site, &node)?;
        Ok(Self { site, node })
    }

    /// Returns whether this valid bundle satisfies only operator-supplied initialization intent:
    /// the requested overrides, and the same execution targets as `composed`, the inputs the
    /// request composes on this host, compared by runtime and devices but not by ID or name.
    pub fn matches_requested_intent(
        &self,
        request: &InitialSiteRequest,
        composed: &InitialSiteInputs,
    ) -> bool {
        if self.contains_runtime_declarations() || self.site.nodes.len() != 1 {
            return false;
        }
        let Some(node) = self
            .site
            .nodes
            .iter()
            .find(|node| node.node_id == self.node.node_id)
        else {
            return false;
        };
        let overrides_match = request
            .site_id_override()
            .is_none_or(|site_id| site_id == self.site.site_id)
            && request
                .node_id_override()
                .is_none_or(|node_id| node_id == node.node_id)
            && request
                .site_node_host_override()
                .is_none_or(|host| node.host == host.to_string())
            && request
                .site_node_display_name_override()
                .is_none_or(|name| name == node.display_name);
        overrides_match
            && declared_choices(&self.node) == declared_choices(&composed.node)
            && target_shapes(&self.site) == target_shapes(&composed.site)
    }

    /// Returns whether runtime-owned declarations are present.
    pub fn contains_runtime_declarations(&self) -> bool {
        contains_runtime_declarations(&self.site)
    }

    /// Returns the complete desired site declaration.
    pub fn site(&self) -> &Site {
        &self.site
    }

    /// Returns the server-local binding for the declared SiteNode.
    pub fn node(&self) -> &NodeConfiguration {
        &self.node
    }

    /// Serializes the desired site using the restricted typed YAML utility.
    pub fn site_yaml(&self) -> Result<String, InitialSiteCompositionError> {
        yaml::to_string(&self.site).map_err(InitialSiteCompositionError::SerializeSiteYaml)
    }

    /// Serializes the server-local node binding using the restricted typed YAML utility.
    pub fn node_yaml(&self) -> Result<String, InitialSiteCompositionError> {
        yaml::to_string(&self.node).map_err(InitialSiteCompositionError::SerializeNodeYaml)
    }
}

/// Failures while composing initial in-memory site inputs.
#[derive(Debug, thiserror::Error)]
pub enum InitialSiteCompositionError {
    /// Docker application runtime was selected without a successful Docker probe.
    #[error("Docker applications selection requires a successful Docker capability probe")]
    MissingDockerProbe,

    /// NVIDIA inference was selected without a successful NVIDIA probe.
    #[error("NVIDIA inference selection requires a successful NVIDIA capability probe")]
    MissingNvidiaProbe,

    /// An explicit selector was not reported by CDI discovery.
    #[error(
        "explicit NVIDIA CDI selector {selector:?} is not in the discovered NVIDIA CDI inventory"
    )]
    UnknownNvidiaCdiSelector {
        /// Selector requested by the initialization request.
        selector: String,
    },

    /// An explicit selector names every NVIDIA device, which a model execution target cannot hold.
    #[error(
        "explicit NVIDIA CDI selector {selector:?} names every GPU; name individual GPUs instead"
    )]
    EveryNvidiaDeviceSelector {
        /// Selector requested by the initialization request.
        selector: String,
    },

    /// Two explicit selectors name the same probed GPU, so their targets would be the same.
    #[error("explicit NVIDIA CDI selectors {other:?} and {selector:?} name the same GPU")]
    SameNvidiaGpu {
        /// Later selector naming the GPU.
        selector: String,

        /// Earlier selector naming the same GPU.
        other: String,
    },

    /// Automatic selection could not map each NVML physical GPU to one full-GPU CDI selector.
    #[error(
        "NVIDIA CDI inventory cannot map every physical GPU to one unambiguous full-GPU selector"
    )]
    AmbiguousNvidiaCdiInventory,

    /// Apple inference was selected without an observed Apple GPU.
    #[error("Apple inference selection requires a successful Apple GPU capability probe")]
    MissingAppleProbe,

    /// Simulated inference was selected beside NVIDIA or Apple inference: a node runs either on
    /// its real hardware or on a declared machine.
    #[error(
        "simulated inference runs on a declared machine and cannot be combined with NVIDIA or Apple \
         inference on one node"
    )]
    SimulatedWithHardwareInference,

    /// Simulated inference names hardware that can't be resolved.
    #[error("simulated inference names hardware that can't be simulated")]
    SimulatedHardware(#[source] MachineError),

    /// The host CPU architecture has no Linux execution platform for simulated applications.
    #[error("Simulated applications need a supported host architecture: {0}")]
    UnsupportedHostPlatform(#[source] ExecutionPlatformError),

    /// A described machine's architecture has no Linux platform for Docker to run.
    #[error("the described machine's architecture has no Linux platform for Docker")]
    DescribedPlatform(#[source] ExecutionPlatformError),

    /// Generated site configuration violated the shared site semantic validation rules.
    #[error("generated initial site is invalid: {0}")]
    InvalidSite(#[source] SiteRepositoryError),

    /// Generated desired site YAML could not be serialized.
    #[error("generated site.yaml could not be serialized: {0}")]
    SerializeSiteYaml(#[source] dotcore_proto::yaml::RestrictedYamlError),

    /// Generated node binding YAML could not be serialized.
    #[error("generated node.yaml could not be serialized: {0}")]
    SerializeNodeYaml(#[source] dotcore_proto::yaml::RestrictedYamlError),
}

/// Failure while loading an existing offline site input bundle.
#[derive(Debug, thiserror::Error)]
pub enum InitialSiteInputLoadError {
    /// Desired site YAML or semantic validation failed.
    #[error("site input load failed: {0}")]
    Site(#[from] SiteRepositoryError),

    /// Server-local node binding YAML could not be decoded.
    #[error("node.yaml load failed: {0}")]
    NodeConfiguration(#[from] dotcore_proto::yaml::RestrictedYamlError),

    /// The binding did not refer to exactly one declared SiteNode.
    #[error("node.yaml binding load failed: {0}")]
    NodeBinding(#[from] NodeBindingError),
}

fn contains_runtime_declarations(site: &Site) -> bool {
    !site.model_deployments.is_empty()
        || !site.application_deployments.is_empty()
        || !site.inference_routes.is_empty()
}

/// The execution targets one SiteNode declares, before their identities and names are derived.
/// Either list may be empty: a node without model targets runs no local models, and one without
/// application targets runs no applications.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeTargetChoices {
    /// Model execution targets, one per choice.
    pub model_targets: Vec<ModelTargetChoice>,

    /// Application execution targets, one per choice.
    pub application_targets: Vec<ApplicationTargetChoice>,

    /// Probed facts of the node's accelerators, which name its model execution targets.
    pub accelerator_facts: Vec<AcceleratorFacts>,

    /// Hardware the node declares in place of its real hardware, when it offers simulated
    /// inference. It stays on the node: a join proposes only the targets it gives.
    pub declared_machine: Option<DeclaredMachine>,
}

/// One model execution target: the device it holds, its runtime and the platform its workloads
/// run in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelTargetChoice {
    /// Simulated models on one GPU of the node's declared machine.
    Simulated {
        /// Platform the simulated workloads stand in for: the declared machine's platform.
        platform: ExecutionPlatform,

        /// ID of the declared machine's GPU the target holds.
        device_id: String,
    },

    /// One NVIDIA GPU, run through Docker.
    Nvidia {
        /// CDI selector naming the GPU.
        selector: String,

        /// Platform Docker runs the workloads in.
        platform: ExecutionPlatform,
    },

    /// The Apple GPU of a macOS node, run natively on darwin/arm64.
    Apple {
        /// Device id the node observed for the GPU.
        device_id: String,

        /// Whether native workloads outlive the server process.
        lifetime: NativeWorkloadLifetime,
    },
}

impl ModelTargetChoice {
    /// Returns the inference capability the target offers.
    pub fn inference(&self) -> InitialInferenceRuntime {
        match self {
            Self::Simulated { .. } => InitialInferenceRuntime::Simulated,
            Self::Nvidia { .. } => InitialInferenceRuntime::Nvidia,
            Self::Apple { .. } => InitialInferenceRuntime::Apple,
        }
    }

    /// Returns the device the target holds.
    pub fn device(&self) -> Option<&str> {
        match self {
            Self::Nvidia { selector, .. } => Some(selector),
            Self::Simulated { device_id, .. } | Self::Apple { device_id, .. } => Some(device_id),
        }
    }

    /// Returns the platform the target's workloads run in.
    pub fn platform(&self) -> ExecutionPlatform {
        match self {
            Self::Simulated { platform, .. } | Self::Nvidia { platform, .. } => platform.clone(),
            Self::Apple { .. } => ExecutionPlatform::darwin_arm64(),
        }
    }
}

/// One application execution target: its runtime, the platform its workloads run in, and the
/// devices it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationTargetChoice {
    /// Application capability the target offers.
    pub applications: InitialApplicationRuntime,

    /// Platform the application workloads run in.
    pub platform: ExecutionPlatform,

    /// CDI devices every workload of the target receives; none for simulated applications.
    pub accelerator_device_ids: Vec<String>,
}

impl NodeTargetChoices {
    /// Resolves the request's capabilities against the probe: simulated inference declares the
    /// request's simulated hardware, resolved for `host_architecture`, and one model target per
    /// GPU of it, on its platform; simulated
    /// applications stand in for Linux on `host_architecture`; Docker-backed targets take the
    /// Docker platform, NVIDIA declares one model target per selected GPU and Apple one target on
    /// the observed GPU. The Docker application target holds every selected NVIDIA GPU. Simulated
    /// inference can't be combined with NVIDIA or Apple inference.
    pub fn resolve(
        request: &InitialSiteRequest,
        probe: &InitialSiteProbeReport,
        host_architecture: &str,
    ) -> Result<Self, InitialSiteCompositionError> {
        let inference = request.inference();
        if inference.contains(&InitialInferenceRuntime::Simulated) && inference.len() > 1 {
            return Err(InitialSiteCompositionError::SimulatedWithHardwareInference);
        }
        let simulated_platform = || {
            ExecutionPlatform::normalize("linux", host_architecture)
                .map_err(InitialSiteCompositionError::UnsupportedHostPlatform)
        };
        let docker_platform = || {
            probe
                .docker()
                .map(|docker| docker.execution_platform().clone())
                .ok_or(InitialSiteCompositionError::MissingDockerProbe)
        };

        let mut choices = Self::default();
        for inference in request.inference() {
            match inference {
                InitialInferenceRuntime::Simulated => {
                    let simulated_choices = request.simulated_hardware();
                    let machine =
                        MachineDescription::simulated(&simulated_choices, host_architecture)
                            .map_err(InitialSiteCompositionError::SimulatedHardware)?;
                    choices
                        .model_targets
                        .extend(
                            machine
                                .gpu_ids()
                                .map(|device_id| ModelTargetChoice::Simulated {
                                    platform: machine.platform().clone(),
                                    device_id: device_id.to_owned(),
                                }),
                        );
                    choices
                        .accelerator_facts
                        .extend(machine.accelerator_facts());
                    choices.declared_machine = Some(DeclaredMachine {
                        choices: simulated_choices,
                        description: Some(machine.to_stored()),
                    });
                }
                InitialInferenceRuntime::Nvidia => {
                    let nvidia = probe
                        .nvidia()
                        .ok_or(InitialSiteCompositionError::MissingNvidiaProbe)?;
                    if !probe.docker().is_some_and(|docker| docker.cdi_enabled()) {
                        return Err(InitialSiteCompositionError::MissingDockerProbe);
                    }
                    let platform = docker_platform()?;
                    for selector in nvidia_accelerator_device_ids(request, probe)? {
                        choices.model_targets.push(ModelTargetChoice::Nvidia {
                            selector,
                            platform: platform.clone(),
                        });
                    }
                    choices
                        .accelerator_facts
                        .extend(nvidia.facts().iter().cloned());
                }
                InitialInferenceRuntime::Apple => {
                    let apple = probe
                        .apple()
                        .ok_or(InitialSiteCompositionError::MissingAppleProbe)?;
                    choices.model_targets.push(ModelTargetChoice::Apple {
                        device_id: apple.device_id().to_owned(),
                        lifetime: request.native_workload_lifetime(),
                    });
                    choices.accelerator_facts.push(apple.facts().clone());
                }
            }
        }
        for applications in request.applications() {
            let platform = match applications {
                InitialApplicationRuntime::Simulated => simulated_platform()?,
                InitialApplicationRuntime::Docker => docker_platform()?,
            };
            let accelerator_device_ids =
                application_accelerator_device_ids(*applications, &choices.model_targets);
            choices.application_targets.push(ApplicationTargetChoice {
                applications: *applications,
                platform,
                accelerator_device_ids,
            });
        }
        Ok(choices)
    }

    /// Resolves the targets a node of `machine` declares when it offers everything its hardware
    /// can: inference on each of its GPUs and Docker applications, resolved as on a host whose
    /// probe found that hardware.
    pub fn described(machine: &MachineDescription) -> Result<Self, InitialSiteCompositionError> {
        let probe = InitialSiteProbeReport::described(machine)
            .map_err(InitialSiteCompositionError::DescribedPlatform)?;
        let nvidia = probe.nvidia().map(|_| InitialInferenceRuntime::Nvidia);
        let apple = probe.apple().map(|_| InitialInferenceRuntime::Apple);
        let request = InitialSiteRequest::new(
            nvidia.into_iter().chain(apple),
            [InitialApplicationRuntime::Docker],
        );
        Self::resolve(&request, &probe, machine.platform().architecture())
    }

    /// Returns how many execution targets the node declares.
    pub fn target_count(&self) -> usize {
        self.model_targets.len() + self.application_targets.len()
    }
}

/// Returns the execution targets node `node_id` declares on a machine of `machine`'s hardware,
/// chosen as site setup chooses them for a host that offers all of it.
pub fn described_execution_targets(
    node_id: &str,
    machine: &MachineDescription,
) -> Result<Vec<ExecutionTarget>, InitialSiteCompositionError> {
    let choices = NodeTargetChoices::described(machine)?;
    Ok(build_execution_targets(node_id, &choices))
}

/// Returns the devices an application target of `applications` holds beside `model_targets`: a
/// Docker target holds every NVIDIA GPU the node offers to models, so GPU applications run wherever
/// a picked GPU does. Simulated targets hold none, and Apple GPUs never reach Docker. Site
/// initialization and node join share it, so both declare the same target.
pub(crate) fn application_accelerator_device_ids(
    applications: InitialApplicationRuntime,
    model_targets: &[ModelTargetChoice],
) -> Vec<String> {
    match applications {
        InitialApplicationRuntime::Simulated => Vec::new(),
        InitialApplicationRuntime::Docker => model_targets
            .iter()
            .filter_map(|target| match target {
                ModelTargetChoice::Nvidia { selector, .. } => Some(selector.clone()),
                ModelTargetChoice::Simulated { .. } | ModelTargetChoice::Apple { .. } => None,
            })
            .collect(),
    }
}

/// Builds the model execution targets of one SiteNode, then its application execution targets.
/// Shared by every flow that declares a SiteNode so the runtime bindings and identities stay
/// identical.
pub(crate) fn build_execution_targets(
    node_id: &str,
    choices: &NodeTargetChoices,
) -> Vec<ExecutionTarget> {
    let model_targets = choices.model_targets.iter().map(move |choice| {
        let adapter = match choice {
            ModelTargetChoice::Simulated { .. } => {
                model_runtime_binding::Adapter::SimulatedRuntime(SimulatedRuntimeAdapter {
                    default_scenario: Some(default_simulated_scenario()),
                    deployment_scenarios: Vec::new(),
                })
            }
            ModelTargetChoice::Nvidia { .. } => {
                model_runtime_binding::Adapter::DockerRuntime(DockerRuntimeAdapter {
                    accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                })
            }
            ModelTargetChoice::Apple { lifetime, .. } => {
                model_runtime_binding::Adapter::NativeRuntime(NativeRuntimeAdapter {
                    workload_lifetime: *lifetime as i32,
                })
            }
        };
        let devices = choice
            .device()
            .map(str::to_owned)
            .into_iter()
            .collect::<Vec<_>>();
        ExecutionTarget {
            execution_target_id: derive_model_execution_target_id(
                node_id,
                choice.inference(),
                choice.device(),
            ),
            site_node_id: node_id.to_owned(),
            display_name: model_target_display_name(
                choice.inference(),
                &devices,
                &choices.accelerator_facts,
            ),
            platform: Some(choice.platform().to_oci_platform()),
            accelerator_device_ids: devices,
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(adapter),
                },
            )),
        }
    });
    let application_targets = choices.application_targets.iter().map(|choice| {
        let adapter = match choice.applications {
            InitialApplicationRuntime::Simulated => {
                application_runtime_binding::Adapter::SimulatedRuntime(
                    SimulatedApplicationRuntimeAdapter {},
                )
            }
            InitialApplicationRuntime::Docker => {
                application_runtime_binding::Adapter::DockerRuntime(
                    DockerApplicationRuntimeAdapter {},
                )
            }
        };
        ExecutionTarget {
            execution_target_id: derive_application_execution_target_id(
                node_id,
                choice.applications,
            ),
            site_node_id: node_id.to_owned(),
            display_name: application_target_display_name(choice.applications),
            platform: Some(choice.platform.to_oci_platform()),
            accelerator_device_ids: choice.accelerator_device_ids.clone(),
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(adapter),
                },
            )),
        }
    });
    model_targets.chain(application_targets).collect()
}

/// Runtime of an execution target as initialization intent sees it: the adapter kind, its
/// accelerator transport or native workload lifetime, and its devices, without IDs or names.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TargetShape {
    SimulatedModel,

    DockerModel {
        transport: i32,
        devices: BTreeSet<String>,
    },

    NativeModel {
        lifetime: i32,
        devices: BTreeSet<String>,
    },

    SimulatedApplications,

    DockerApplications {
        devices: BTreeSet<String>,
    },

    /// A target initialization never declares, such as one without a runtime binding.
    Other,
}

/// Returns the shapes of every execution target in `site`, sorted so that order doesn't count.
/// The simulated hardware choices a node configuration was resolved from, if it declares any: the
/// operator's intent, compared instead of the resolved description, which follows the data.
fn declared_choices(node: &NodeConfiguration) -> Option<&[String]> {
    node.declared_machine
        .as_ref()
        .map(|machine| machine.choices.as_slice())
}

fn target_shapes(site: &Site) -> Vec<TargetShape> {
    let mut shapes = site
        .execution_targets
        .iter()
        .map(|target| {
            let devices = || target.accelerator_device_ids.iter().cloned().collect();
            match &target.runtime_binding {
                Some(execution_target::RuntimeBinding::ModelRuntime(binding)) => {
                    match &binding.adapter {
                        Some(model_runtime_binding::Adapter::SimulatedRuntime(_)) => {
                            TargetShape::SimulatedModel
                        }
                        Some(model_runtime_binding::Adapter::DockerRuntime(settings)) => {
                            TargetShape::DockerModel {
                                transport: settings.accelerator_transport,
                                devices: devices(),
                            }
                        }
                        Some(model_runtime_binding::Adapter::NativeRuntime(settings)) => {
                            TargetShape::NativeModel {
                                lifetime: normalized_lifetime(settings.workload_lifetime),
                                devices: devices(),
                            }
                        }
                        None => TargetShape::Other,
                    }
                }
                Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) => {
                    match &binding.adapter {
                        Some(application_runtime_binding::Adapter::SimulatedRuntime(_)) => {
                            TargetShape::SimulatedApplications
                        }
                        Some(application_runtime_binding::Adapter::DockerRuntime(_)) => {
                            TargetShape::DockerApplications { devices: devices() }
                        }
                        None => TargetShape::Other,
                    }
                }
                None => TargetShape::Other,
            }
        })
        .collect::<Vec<_>>();
    shapes.sort();
    shapes
}

/// Returns a stored native workload lifetime, reading unspecified as persistent.
fn normalized_lifetime(lifetime: i32) -> i32 {
    match NativeWorkloadLifetime::try_from(lifetime) {
        Ok(NativeWorkloadLifetime::Unspecified) => NativeWorkloadLifetime::Persistent as i32,
        _ => lifetime,
    }
}

fn default_simulated_scenario() -> SimulatedRuntimeScenario {
    SimulatedRuntimeScenario {
        ready_delay_millis: SIMULATED_READY_DELAY_MILLIS,
        stop_delay_millis: SIMULATED_STOP_DELAY_MILLIS,
    }
}

/// Returns the NVIDIA CDI selectors a node declares, one model execution target each: the request's
/// explicit selectors checked against the probe, or one full-GPU selector per device.
pub fn nvidia_accelerator_device_ids(
    request: &InitialSiteRequest,
    probe: &InitialSiteProbeReport,
) -> Result<Vec<String>, InitialSiteCompositionError> {
    let nvidia = probe
        .nvidia()
        .ok_or(InitialSiteCompositionError::MissingNvidiaProbe)?;
    let discovered = nvidia.cdi_selectors().collect::<BTreeSet<_>>();
    match request.nvidia_cdi_selectors() {
        Some(selectors) => explicit_nvidia_selectors(selectors, &discovered, nvidia.facts()),
        None => automatic_nvidia_selectors(nvidia.facts(), &discovered),
    }
}

/// Checks explicit selectors, kept in request order without repeats: each names one discovered
/// device other than every device, and no two name the same probed GPU.
fn explicit_nvidia_selectors(
    selectors: &[String],
    discovered: &BTreeSet<&str>,
    facts: &[AcceleratorFacts],
) -> Result<Vec<String>, InitialSiteCompositionError> {
    let mut selected = Vec::<String>::new();
    for selector in selectors {
        if names_every_cdi_device(selector) {
            return Err(InitialSiteCompositionError::EveryNvidiaDeviceSelector {
                selector: selector.clone(),
            });
        }
        if !discovered.contains(selector.as_str()) {
            return Err(InitialSiteCompositionError::UnknownNvidiaCdiSelector {
                selector: selector.clone(),
            });
        }
        if !selected.contains(selector) {
            selected.push(selector.clone());
        }
    }
    if let Some((other, selector)) = same_gpu_selectors(&selected, facts) {
        return Err(InitialSiteCompositionError::SameNvidiaGpu {
            selector: selector.to_owned(),
            other: other.to_owned(),
        });
    }
    if selected.is_empty() {
        return Err(InitialSiteCompositionError::AmbiguousNvidiaCdiInventory);
    }
    Ok(selected)
}

/// Returns the first two selectors, earlier one first, that name the same GPU among the probed
/// facts. Selectors that name no single known GPU, such as MIG slices, are never compared, since
/// their targets are told apart by their device names.
pub(crate) fn same_gpu_selectors<'a>(
    selectors: &'a [String],
    facts: &[AcceleratorFacts],
) -> Option<(&'a str, &'a str)> {
    let mut gpus = BTreeMap::new();
    for selector in selectors {
        if let Some([gpu]) =
            selected_accelerator_facts(std::slice::from_ref(selector), facts).as_deref()
            && let Some(other) = gpus.insert(gpu.device_id.as_str(), selector.as_str())
        {
            return Some((other, selector));
        }
    }
    None
}

/// Picks one full-GPU selector per probed GPU in enumeration order: `gpuN`, else the bare index,
/// else the GPU's UUID when the CDI inventory names GPUs only by UUID.
fn automatic_nvidia_selectors(
    facts: &[AcceleratorFacts],
    discovered: &BTreeSet<&str>,
) -> Result<Vec<String>, InitialSiteCompositionError> {
    let mut aliases = BTreeMap::<u32, BTreeSet<&str>>::new();
    for selector in discovered {
        if let Some(index) = nvidia_cdi_gpu_index(selector) {
            aliases.entry(index).or_default().insert(selector);
        }
    }
    let device_count = u32::try_from(facts.len())
        .map_err(|_| InitialSiteCompositionError::AmbiguousNvidiaCdiInventory)?;
    if aliases.keys().any(|index| *index >= device_count) {
        return Err(InitialSiteCompositionError::AmbiguousNvidiaCdiInventory);
    }
    let mut selected = Vec::new();
    for index in 0..device_count {
        let canonical = nvidia_cdi_gpu_selector(index);
        let legacy = nvidia_cdi_legacy_gpu_selector(index);
        let indexed = aliases.get(&index).and_then(|candidates| {
            candidates
                .get(canonical.as_str())
                .or_else(|| candidates.get(legacy.as_str()))
                .map(|selector| (*selector).to_owned())
        });
        let selector = match indexed {
            Some(selector) => selector,
            None if !aliases.contains_key(&index) => uuid_selector(index, facts, discovered)?,
            None => return Err(InitialSiteCompositionError::AmbiguousNvidiaCdiInventory),
        };
        selected.push(selector);
    }
    Ok(selected)
}

/// Returns the discovered UUID selector of the one GPU enumerated at `index`.
fn uuid_selector(
    index: u32,
    facts: &[AcceleratorFacts],
    discovered: &BTreeSet<&str>,
) -> Result<String, InitialSiteCompositionError> {
    let [fact] = facts
        .iter()
        .filter(|fact| fact.vendor_index == Some(index))
        .collect::<Vec<_>>()[..]
    else {
        return Err(InitialSiteCompositionError::AmbiguousNvidiaCdiInventory);
    };
    let selector = format!("nvidia.com/gpu={}", fact.device_id);
    if discovered.contains(selector.as_str()) {
        Ok(selector)
    } else {
        Err(InitialSiteCompositionError::AmbiguousNvidiaCdiInventory)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use dotcore_domain::SiteNodeHost;
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::{
        node::v1::DeclaredMachine,
        observation::v1::{AcceleratorFacts, AcceleratorVendor},
        site::v1::{execution_target, model_runtime_binding},
    };
    use dotcore_proto::{dotcore::site::v1::Site, yaml};

    use super::{InitialSiteInputs, declared_choices};
    use crate::initial::{
        probe::{test_apple_probe_report, test_probe_report, test_probe_report_for_platform},
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentityGenerator,
            InitialSiteIdentityKind, InitialSiteRequest, derive_application_execution_target_id,
            derive_model_execution_target_id,
        },
    };

    const RTX_5090: &str = "NVIDIA GeForce RTX 5090";

    #[test]
    fn composes_one_typed_target_per_offered_capability() {
        use InitialApplicationRuntime as Applications;
        use InitialInferenceRuntime as Inference;

        for (inference, applications, names) in [
            (
                vec![Inference::Simulated],
                vec![Applications::Simulated],
                vec![
                    "Simulated NVIDIA GeForce RTX 5090",
                    "Simulated applications",
                ],
            ),
            (
                vec![Inference::Simulated],
                vec![Applications::Docker],
                vec!["Simulated NVIDIA GeForce RTX 5090", "Docker applications"],
            ),
            (
                vec![Inference::Nvidia],
                vec![Applications::Simulated],
                vec![RTX_5090, "Simulated applications"],
            ),
            (
                vec![Inference::Nvidia],
                vec![Applications::Docker, Applications::Simulated],
                vec![RTX_5090, "Simulated applications", "Docker applications"],
            ),
            (vec![Inference::Nvidia], vec![], vec![RTX_5090]),
            (
                vec![],
                vec![Applications::Docker],
                vec!["Docker applications"],
            ),
            (vec![], vec![], vec![]),
        ] {
            let nvidia = inference.contains(&Inference::Nvidia);
            let docker = nvidia || applications.contains(&Applications::Docker);
            let request = InitialSiteRequest::new(inference, applications);
            let inputs = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report(docker, nvidia.then(nvidia_facts), ["nvidia.com/gpu=gpu0"]),
            )
            .expect("valid initial inputs");

            assert_eq!(target_names(&inputs), names);
            let ids = inputs
                .site()
                .execution_targets
                .iter()
                .map(|target| target.execution_target_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(ids.len(), names.len(), "distinct target IDs");
            assert!(
                inputs
                    .site()
                    .execution_targets
                    .iter()
                    .all(|target| { target.site_node_id == inputs.site().nodes[0].node_id })
            );
            assert_eq!(inputs.node().node_id, inputs.site().nodes[0].node_id);
            assert!(inputs.matches_requested_intent(&request, &inputs));
        }
    }

    #[test]
    fn simulated_inference_declares_its_machine_and_runs_on_its_gpus() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Docker],
        )
        .with_simulated_hardware(vec!["mac-64g".to_owned()]);
        let inputs = InitialSiteInputs::compose_for_architecture(
            &request,
            identities(&request),
            &test_probe_report_for_platform(
                Some(dotcore_domain::ExecutionPlatform::linux_amd64()),
                None,
                std::iter::empty::<&str>(),
            ),
            "x86_64",
        )
        .expect("declared Mac with Docker applications on a Linux host");

        let node_id = &inputs.site().nodes[0].node_id;
        assert_eq!(
            inputs.node().declared_machine,
            Some(DeclaredMachine {
                choices: vec!["mac-64g".to_owned()],
                description: Some(
                    MachineDescription::named("mac-64g")
                        .expect("named machine")
                        .to_stored()
                ),
            })
        );
        assert_eq!(
            target_details(&inputs),
            [
                (
                    derive_model_execution_target_id(
                        node_id,
                        InitialInferenceRuntime::Simulated,
                        Some("apple-gpu-0")
                    ),
                    "Simulated Apple M4 Max",
                    dotcore_domain::ExecutionPlatform::darwin_arm64(),
                    vec!["apple-gpu-0".to_owned()],
                    "simulated",
                ),
                (
                    derive_application_execution_target_id(
                        node_id,
                        InitialApplicationRuntime::Docker
                    ),
                    "Docker applications",
                    dotcore_domain::ExecutionPlatform::linux_amd64(),
                    Vec::new(),
                    "docker",
                ),
            ]
        );
        assert!(inputs.matches_requested_intent(&request, &inputs));
    }

    #[test]
    fn a_described_machine_declares_the_targets_its_hardware_offers() {
        use dotcore_domain::ExecutionPlatform;

        let mac = MachineDescription::named("mac-64g").expect("named machine");
        let two_gpus = MachineDescription::from_yaml("two-gpus", TWO_NVIDIA_GPUS).expect("machine");
        let no_gpu = MachineDescription::from_yaml("no-gpu", NO_GPU).expect("machine");
        let selectors = ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"].map(str::to_owned);

        // A Mac runs models natively on its GPU and applications in Docker Desktop's Linux.
        assert_eq!(
            described_details("node-a", &mac),
            [
                (
                    derive_model_execution_target_id(
                        "node-a",
                        InitialInferenceRuntime::Apple,
                        Some("apple-gpu-0")
                    ),
                    "Apple M4 Max".to_owned(),
                    ExecutionPlatform::darwin_arm64(),
                    vec!["apple-gpu-0".to_owned()],
                    "native",
                ),
                (
                    derive_application_execution_target_id(
                        "node-a",
                        InitialApplicationRuntime::Docker
                    ),
                    "Docker applications".to_owned(),
                    ExecutionPlatform::linux_arm64(),
                    Vec::new(),
                    "docker",
                ),
            ]
        );

        // Each NVIDIA GPU gets its own Docker target, and the application target holds them all.
        let gpu_target = |index: usize| {
            (
                derive_model_execution_target_id(
                    "node-b",
                    InitialInferenceRuntime::Nvidia,
                    Some(&selectors[index]),
                ),
                format!("{RTX_5090} · GPU {index}"),
                ExecutionPlatform::linux_amd64(),
                vec![selectors[index].clone()],
                "docker",
            )
        };
        assert_eq!(
            described_details("node-b", &two_gpus),
            [
                gpu_target(0),
                gpu_target(1),
                (
                    derive_application_execution_target_id(
                        "node-b",
                        InitialApplicationRuntime::Docker
                    ),
                    "Docker applications".to_owned(),
                    ExecutionPlatform::linux_amd64(),
                    selectors.to_vec(),
                    "docker",
                ),
            ]
        );

        assert_eq!(
            described_details("node-c", &no_gpu),
            [(
                derive_application_execution_target_id("node-c", InitialApplicationRuntime::Docker),
                "Docker applications".to_owned(),
                ExecutionPlatform::linux_arm64(),
                Vec::new(),
                "docker",
            )]
        );
    }

    #[test]
    fn simulated_inference_declares_one_target_per_chosen_gpu_in_order() {
        let default = compose(&simulated_request());
        let mixed = compose(
            &simulated_request()
                .with_simulated_hardware(vec!["rtx-4090".to_owned(), "rtx-4080".to_owned()]),
        );

        assert_eq!(InitialSiteRequest::DEFAULT_SIMULATED_HARDWARE, "rtx-5090");
        assert_eq!(
            declared_choices(default.node()),
            Some(&["rtx-5090".to_owned()][..])
        );
        assert_eq!(
            model_target_devices(&default),
            [(
                "Simulated NVIDIA GeForce RTX 5090",
                vec!["GPU-00000000-0000-0000-0000-000000005090".to_owned()]
            )]
        );
        assert_eq!(
            declared_choices(mixed.node()),
            Some(&["rtx-4090".to_owned(), "rtx-4080".to_owned()][..])
        );
        assert_eq!(
            model_target_devices(&mixed),
            [
                (
                    "Simulated NVIDIA GeForce RTX 4090 · GPU 0",
                    vec!["GPU-00000000-0000-0000-0000-000000004090".to_owned()]
                ),
                (
                    "Simulated NVIDIA GeForce RTX 4080 · GPU 1",
                    vec!["GPU-00000000-0000-0000-0000-000000004080".to_owned()]
                ),
            ]
        );
        let other_hardware = simulated_request().with_simulated_hardware(vec!["gb10".to_owned()]);
        assert!(!default.matches_requested_intent(&other_hardware, &compose(&other_hardware)));
    }

    #[test]
    fn a_simulated_node_keeps_its_stored_hardware_when_the_data_changes() {
        let request = simulated_request();
        let mut stored = compose(&request);
        let description = stored
            .node
            .declared_machine
            .as_mut()
            .and_then(|machine| machine.description.as_mut())
            .expect("stored description");
        description.cpu_count = 3;
        description.host_memory_total_bytes /= 2;

        assert!(stored.matches_requested_intent(&request, &compose(&request)));
        assert_eq!(
            stored
                .node()
                .declared_machine
                .as_ref()
                .and_then(|machine| machine.description.as_ref())
                .map(|description| description.cpu_count),
            Some(3)
        );
    }

    #[test]
    fn simulated_inference_is_refused_beside_nvidia_or_apple_and_on_an_unknown_machine() {
        for (hardware, probe) in [
            (
                InitialInferenceRuntime::Nvidia,
                test_probe_report(true, Some(nvidia_facts()), ["nvidia.com/gpu=gpu0"]),
            ),
            (
                InitialInferenceRuntime::Apple,
                test_apple_probe_report("apple-gpu-0"),
            ),
        ] {
            let request = InitialSiteRequest::new(
                [hardware, InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Simulated],
            );
            let error = InitialSiteInputs::compose(&request, identities(&request), &probe)
                .expect_err("simulated beside hardware inference");

            assert_eq!(
                error.to_string(),
                "simulated inference runs on a declared machine and cannot be combined with \
                 NVIDIA or Apple inference on one node"
            );
            assert!(matches!(
                error,
                super::InitialSiteCompositionError::SimulatedWithHardwareInference
            ));
        }

        let unknown = simulated_request().with_simulated_hardware(vec!["rtx-9090".to_owned()]);
        let error = InitialSiteInputs::compose(
            &unknown,
            identities(&unknown),
            &test_probe_report(false, None, std::iter::empty::<&str>()),
        )
        .expect_err("unknown hardware");
        assert_eq!(
            error.to_string(),
            "simulated inference names hardware that can't be simulated"
        );
        assert!(matches!(
            error,
            super::InitialSiteCompositionError::SimulatedHardware(
                dotcore_node_hardware::MachineError::UnknownSimulatedChoice { .. }
            )
        ));
    }

    #[test]
    fn an_empty_request_declares_a_node_without_targets() {
        let empty = InitialSiteRequest::new([], []);
        let inputs = compose(&empty);

        assert!(inputs.site().execution_targets.is_empty());
        assert_eq!(inputs.site().nodes.len(), 1);
        assert!(inputs.matches_requested_intent(&empty, &inputs));
        let simulated = simulated_request();
        assert!(!inputs.matches_requested_intent(&simulated, &compose(&simulated)));
    }

    #[test]
    fn apple_target_is_native_darwin_with_the_observed_device_and_lifetime() {
        use dotcore_proto::dotcore::site::v1::{
            NativeRuntimeAdapter, NativeWorkloadLifetime, model_runtime_binding,
        };

        for (lifetime, device) in [
            (NativeWorkloadLifetime::Persistent, "apple-gpu-0"),
            (NativeWorkloadLifetime::Ephemeral, "apple-gpu-0"),
        ] {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Apple],
                [InitialApplicationRuntime::Simulated],
            )
            .with_native_workload_lifetime(lifetime);
            let inputs = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_apple_probe_report(device),
            )
            .expect("Apple inputs");

            let target = inputs
                .site()
                .execution_targets
                .iter()
                .find(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ModelRuntime(_))
                    )
                })
                .expect("Model target");
            assert_eq!(
                target.platform,
                Some(dotcore_domain::ExecutionPlatform::darwin_arm64().to_oci_platform())
            );
            assert_eq!(target.accelerator_device_ids, [device]);
            assert_eq!(target.display_name, "Apple M3 Max");
            assert_eq!(
                target.runtime_binding,
                Some(execution_target::RuntimeBinding::ModelRuntime(
                    dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                        adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                            NativeRuntimeAdapter {
                                workload_lifetime: lifetime as i32,
                            }
                        )),
                    }
                ))
            );
            assert!(inputs.matches_requested_intent(&request, &inputs));
            let other = request.clone().with_native_workload_lifetime(
                if lifetime == NativeWorkloadLifetime::Persistent {
                    NativeWorkloadLifetime::Ephemeral
                } else {
                    NativeWorkloadLifetime::Persistent
                },
            );
            let other_inputs = InitialSiteInputs::compose(
                &other,
                identities(&other),
                &test_apple_probe_report(device),
            )
            .expect("other Apple inputs");
            assert!(!inputs.matches_requested_intent(&other, &other_inputs));
        }
    }

    #[test]
    fn nvidia_default_declares_one_named_model_target_per_gpu_and_docker_applications_hold_them() {
        for (count, discovered, selectors, names) in [
            (
                1,
                vec![
                    "nvidia.com/gpu=all",
                    "nvidia.com/gpu=gpu0",
                    "nvidia.com/gpu=0",
                ],
                vec!["nvidia.com/gpu=gpu0"],
                vec![RTX_5090.to_owned()],
            ),
            (
                2,
                vec![
                    "nvidia.com/gpu=all",
                    "nvidia.com/gpu=gpu0",
                    "nvidia.com/gpu=gpu1",
                    "nvidia.com/gpu=GPU-fixture-0",
                    "nvidia.com/gpu=GPU-fixture-1",
                ],
                vec!["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"],
                vec![format!("{RTX_5090} · GPU 0"), format!("{RTX_5090} · GPU 1")],
            ),
            (
                3,
                vec!["nvidia.com/gpu=0", "nvidia.com/gpu=1", "nvidia.com/gpu=2"],
                vec!["nvidia.com/gpu=0", "nvidia.com/gpu=1", "nvidia.com/gpu=2"],
                vec![
                    format!("{RTX_5090} · GPU 0"),
                    format!("{RTX_5090} · GPU 1"),
                    format!("{RTX_5090} · GPU 2"),
                ],
            ),
            (
                2,
                vec![
                    "nvidia.com/gpu=all",
                    "nvidia.com/gpu=GPU-fixture-0",
                    "nvidia.com/gpu=GPU-fixture-1",
                ],
                vec![
                    "nvidia.com/gpu=GPU-fixture-0",
                    "nvidia.com/gpu=GPU-fixture-1",
                ],
                vec![format!("{RTX_5090} · GPU 0"), format!("{RTX_5090} · GPU 1")],
            ),
        ] {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Docker],
            );
            let inputs = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report(true, Some(nvidia_facts_with_count(count)), discovered),
            )
            .expect("NVIDIA inputs");

            let node_id = &inputs.site().nodes[0].node_id;
            let mut expected = selectors
                .iter()
                .zip(&names)
                .map(|(selector, name)| {
                    (
                        derive_model_execution_target_id(
                            node_id,
                            InitialInferenceRuntime::Nvidia,
                            Some(selector),
                        ),
                        vec![(*selector).to_owned()],
                        name.clone(),
                    )
                })
                .collect::<Vec<_>>();
            expected.push((
                derive_application_execution_target_id(node_id, InitialApplicationRuntime::Docker),
                selectors
                    .iter()
                    .map(|selector| (*selector).to_owned())
                    .collect(),
                "Docker applications".to_owned(),
            ));
            assert_eq!(target_summaries(&inputs), expected, "{selectors:?}");
            assert!(inputs.matches_requested_intent(&request, &inputs));
        }
    }

    #[test]
    fn nvidia_default_without_one_selector_per_gpu_is_ambiguous() {
        for (count, discovered) in [
            (2, vec!["nvidia.com/gpu=gpu0"]),
            (1, vec!["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"]),
            (1, vec!["nvidia.com/gpu=all", "nvidia.com/gpu=GPU-other"]),
        ] {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Docker],
            );
            let error = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report(true, Some(nvidia_facts_with_count(count)), discovered),
            )
            .expect_err("ambiguous inventory");

            assert!(matches!(
                error,
                super::InitialSiteCompositionError::AmbiguousNvidiaCdiInventory
            ));
        }
    }

    #[test]
    fn explicit_nvidia_selectors_declare_one_target_each() {
        let mig = "nvidia.com/gpu=MIG-f9a4a3aa-f02d-97ca-9972-3f61413b228b";
        let discovered = [
            "nvidia.com/gpu=all",
            "nvidia.com/gpu=gpu0",
            "nvidia.com/gpu=gpu1",
            "nvidia.com/gpu=0",
            "nvidia.com/gpu=GPU-fixture-1",
            mig,
        ];
        for (selectors, names) in [
            (
                vec!["nvidia.com/gpu=gpu1"],
                vec![format!("{RTX_5090} · GPU 1")],
            ),
            (
                vec!["nvidia.com/gpu=gpu1", "nvidia.com/gpu=0"],
                vec![format!("{RTX_5090} · GPU 1"), format!("{RTX_5090} · GPU 0")],
            ),
            (vec![mig], vec![format!("NVIDIA GPU · {}", &mig[15..])]),
        ] {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Docker],
            )
            .with_nvidia_cdi_selectors(selectors.iter().copied());
            let inputs = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report(true, Some(nvidia_facts_with_count(2)), discovered),
            )
            .expect("explicit NVIDIA inputs");

            let summaries = target_summaries(&inputs);
            let model_names = summaries[..summaries.len() - 1]
                .iter()
                .map(|(_, _, name)| name.clone())
                .collect::<Vec<_>>();
            assert_eq!(model_names, names, "{selectors:?}");
            assert!(
                summaries
                    .iter()
                    .all(|(id, _, _)| dotcore_domain::valid_id(id))
            );
            assert!(inputs.matches_requested_intent(&request, &inputs));
        }
    }

    #[test]
    fn explicit_nvidia_selectors_name_single_distinct_gpus() {
        let discovered = [
            "nvidia.com/gpu=all",
            "nvidia.com/gpu=gpu0",
            "nvidia.com/gpu=0",
            "nvidia.com/gpu=GPU-fixture-0",
        ];
        for (selectors, expected) in [
            (
                vec!["nvidia.com/gpu=0", "nvidia.com/gpu=gpu0"],
                "explicit NVIDIA CDI selectors \"nvidia.com/gpu=0\" and \"nvidia.com/gpu=gpu0\" name the same GPU",
            ),
            (
                vec!["nvidia.com/gpu=gpu0", "nvidia.com/gpu=GPU-fixture-0"],
                "explicit NVIDIA CDI selectors \"nvidia.com/gpu=gpu0\" and \"nvidia.com/gpu=GPU-fixture-0\" name the same GPU",
            ),
            (
                vec!["nvidia.com/gpu=all"],
                "explicit NVIDIA CDI selector \"nvidia.com/gpu=all\" names every GPU; name individual GPUs instead",
            ),
        ] {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Docker],
            )
            .with_nvidia_cdi_selectors(selectors.iter().copied());
            let error = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report(true, Some(nvidia_facts_with_count(2)), discovered),
            )
            .expect_err("rejected selectors");

            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn intent_compares_targets_by_runtime_and_devices() {
        let discovered = ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"];
        let compose_nvidia = |request: &InitialSiteRequest| {
            InitialSiteInputs::compose(
                request,
                identities(request),
                &test_probe_report(true, Some(nvidia_facts_with_count(2)), discovered),
            )
            .expect("NVIDIA inputs")
        };
        let default = InitialSiteRequest::new(
            [InitialInferenceRuntime::Nvidia],
            [InitialApplicationRuntime::Docker],
        );
        let reordered = default
            .clone()
            .with_nvidia_cdi_selectors(["nvidia.com/gpu=gpu1", "nvidia.com/gpu=gpu0"]);
        let existing = compose_nvidia(&default);

        assert!(existing.matches_requested_intent(&default, &compose_nvidia(&default)));
        assert!(existing.matches_requested_intent(&reordered, &compose_nvidia(&reordered)));

        let mut all_gpus = existing.clone();
        all_gpus.site.execution_targets.remove(1);
        all_gpus.site.execution_targets[0].accelerator_device_ids =
            discovered.map(str::to_owned).to_vec();
        assert!(!all_gpus.matches_requested_intent(&default, &existing));

        let mut with_pair = existing.clone();
        let mut pair = with_pair.site.execution_targets[0].clone();
        pair.accelerator_device_ids = discovered.map(str::to_owned).to_vec();
        with_pair.site.execution_targets.push(pair);
        assert!(!with_pair.matches_requested_intent(&default, &existing));

        let one_gpu = default
            .clone()
            .with_nvidia_cdi_selectors(["nvidia.com/gpu=gpu1"]);
        assert!(!existing.matches_requested_intent(&one_gpu, &compose_nvidia(&one_gpu)));

        let simulated_applications = InitialSiteRequest::new(
            [InitialInferenceRuntime::Nvidia],
            [InitialApplicationRuntime::Simulated],
        );
        assert!(!existing.matches_requested_intent(
            &simulated_applications,
            &compose_nvidia(&simulated_applications)
        ));

        let simulated = simulated_request();
        assert!(!existing.matches_requested_intent(&simulated, &compose(&simulated)));
    }

    #[test]
    fn apple_inference_without_a_probe_is_rejected() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Apple],
            [InitialApplicationRuntime::Simulated],
        );

        let error = InitialSiteInputs::compose(
            &request,
            identities(&request),
            &test_probe_report(false, None, std::iter::empty::<&str>()),
        )
        .expect_err("missing Apple probe");

        assert!(matches!(
            error,
            super::InitialSiteCompositionError::MissingAppleProbe
        ));
    }

    #[test]
    fn simulated_applications_follow_the_host_architecture_and_models_the_machine() {
        let request = simulated_request().with_simulated_hardware(vec!["gb10".to_owned()]);
        for (architecture, applications) in [
            ("x86_64", dotcore_domain::ExecutionPlatform::linux_amd64()),
            ("aarch64", dotcore_domain::ExecutionPlatform::linux_arm64()),
        ] {
            let inputs = InitialSiteInputs::compose_for_architecture(
                &request,
                identities(&request),
                &test_probe_report(false, None, std::iter::empty::<&str>()),
                architecture,
            )
            .expect("simulated inputs");

            let platforms = inputs
                .site()
                .execution_targets
                .iter()
                .map(|target| target.platform.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                platforms,
                [
                    Some(dotcore_domain::ExecutionPlatform::linux_arm64().to_oci_platform()),
                    Some(applications.to_oci_platform()),
                ],
                "{architecture}"
            );
        }

        let error = InitialSiteInputs::compose_for_architecture(
            &request,
            identities(&request),
            &test_probe_report(false, None, std::iter::empty::<&str>()),
            "riscv64",
        )
        .expect_err("unsupported architecture");
        assert!(matches!(
            error,
            super::InitialSiteCompositionError::UnsupportedHostPlatform(_)
        ));
    }

    #[test]
    fn docker_targets_use_the_probed_platform() {
        for platform in [
            dotcore_domain::ExecutionPlatform::linux_amd64(),
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        ] {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Docker],
            );
            let inputs = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report_for_platform(
                    Some(platform.clone()),
                    None,
                    std::iter::empty::<&str>(),
                ),
            )
            .expect("Docker application target");
            let target = inputs
                .site()
                .execution_targets
                .iter()
                .find(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                    )
                })
                .expect("Application target");
            assert_eq!(target.platform, Some(platform.to_oci_platform()));
        }
    }

    #[test]
    fn nvidia_target_uses_exact_selected_devices_and_cdi() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Nvidia],
            [InitialApplicationRuntime::Simulated],
        )
        .with_nvidia_cdi_selectors(["nvidia.com/gpu=gpu1"]);
        let inputs = InitialSiteInputs::compose(
            &request,
            identities(&request),
            &test_probe_report(
                true,
                Some(nvidia_facts_with_count(2)),
                ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"],
            ),
        )
        .expect("NVIDIA inputs");
        let target = inputs
            .site()
            .execution_targets
            .iter()
            .find(|target| {
                matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ModelRuntime(_))
                )
            })
            .expect("model target");
        assert_eq!(target.accelerator_device_ids, ["nvidia.com/gpu=gpu1"]);
        let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
            target.runtime_binding.as_ref()
        else {
            unreachable!()
        };
        assert!(matches!(
            binding.adapter,
            Some(model_runtime_binding::Adapter::DockerRuntime(_))
        ));
        assert!(inputs.matches_requested_intent(&request, &inputs));
    }

    #[test]
    fn docker_application_target_holds_the_gpus_picked_for_models() {
        let compose = |request: &InitialSiteRequest| {
            InitialSiteInputs::compose(
                request,
                identities(request),
                &test_probe_report(
                    true,
                    Some(nvidia_facts_with_count(2)),
                    ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"],
                ),
            )
            .expect("inputs")
        };
        let application_devices = |inputs: &InitialSiteInputs| {
            inputs
                .site()
                .execution_targets
                .iter()
                .filter(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                    )
                })
                .map(|target| target.accelerator_device_ids.clone())
                .collect::<Vec<_>>()
        };
        let every_gpu = InitialSiteRequest::new(
            [InitialInferenceRuntime::Nvidia],
            [InitialApplicationRuntime::Docker],
        );
        let picked_gpu = every_gpu
            .clone()
            .with_nvidia_cdi_selectors(["nvidia.com/gpu=gpu1"]);
        let simulated_models = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [
                InitialApplicationRuntime::Docker,
                InitialApplicationRuntime::Simulated,
            ],
        );

        let every = compose(&every_gpu);
        let picked = compose(&picked_gpu);
        let deviceless = compose(&simulated_models);

        assert_eq!(
            application_devices(&every),
            [vec![
                "nvidia.com/gpu=gpu0".to_owned(),
                "nvidia.com/gpu=gpu1".to_owned()
            ]]
        );
        assert_eq!(
            application_devices(&picked),
            [vec!["nvidia.com/gpu=gpu1".to_owned()]]
        );
        assert_eq!(
            application_devices(&deviceless),
            [Vec::<String>::new(), Vec::new()]
        );
        assert!(picked.matches_requested_intent(&picked_gpu, &picked));
        assert!(!every.matches_requested_intent(&picked_gpu, &picked));
    }

    #[test]
    fn site_node_host_override_is_retained() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
        .with_site_node_host_override("192.0.2.10")
        .expect("specific IPv4 override");
        let inputs = InitialSiteInputs::compose(
            &request,
            identities(&request),
            &test_probe_report(false, None, std::iter::empty::<&str>()),
        )
        .expect("valid initial inputs");
        assert_eq!(inputs.site().nodes[0].host, "192.0.2.10");
    }

    #[test]
    fn node_display_name_prefers_explicit_then_default_then_manager() {
        let base = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        );
        for (request, default, expected) in [
            (
                base.clone().with_site_node_display_name_override("Chosen"),
                Some("studio"),
                "Chosen",
            ),
            (base.clone(), Some("studio"), "studio"),
            (base.clone(), None, "manager"),
        ] {
            assert_eq!(
                compose_with_default(&request, default).site().nodes[0].display_name,
                expected
            );
        }
    }

    #[test]
    fn explicit_node_display_name_is_stored_trimmed() {
        let request = simulated_request().with_site_node_display_name_override("  Chosen \n");

        assert_eq!(request.site_node_display_name_override(), Some("Chosen"));
        assert_eq!(compose(&request).site().nodes[0].display_name, "Chosen");
    }

    #[test]
    fn invalid_explicit_node_display_name_fails_composition() {
        for name in ["", "   ", &"a".repeat(129), "bad\u{7}name"] {
            let request = simulated_request().with_site_node_display_name_override(name);
            let error = InitialSiteInputs::compose(
                &request,
                identities(&request),
                &test_probe_report(false, None, std::iter::empty::<&str>()),
            )
            .expect_err("invalid name");

            assert!(
                matches!(error, super::InitialSiteCompositionError::InvalidSite(_)),
                "name {name:?}: {error:?}"
            );
        }
    }

    #[test]
    fn matching_compares_only_an_explicit_node_display_name() {
        let existing = compose(&simulated_request().with_site_node_display_name_override("Legacy"));

        for (request, expected) in [
            (simulated_request(), true),
            (
                simulated_request().with_site_node_display_name_override("Legacy"),
                true,
            ),
            (
                simulated_request().with_site_node_display_name_override("Other"),
                false,
            ),
        ] {
            assert_eq!(
                existing.matches_requested_intent(&request, &compose(&request)),
                expected
            );
        }
    }

    #[test]
    fn matching_compares_only_an_explicit_node_host() {
        let tailnet_name = SiteNodeHost::parse("gpu1.example.ts.net").expect("name");
        let existing =
            compose(&simulated_request().with_site_node_host_default(tailnet_name.clone()));
        assert_eq!(existing.site().nodes[0].host, "gpu1.example.ts.net");

        for (request, expected) in [
            (simulated_request(), true),
            (
                simulated_request().with_site_node_host_default(
                    SiteNodeHost::parse("100.101.102.104").expect("address"),
                ),
                true,
            ),
            (
                simulated_request()
                    .with_site_node_host_override("gpu1.example.ts.net")
                    .expect("host"),
                true,
            ),
            (
                simulated_request()
                    .with_site_node_host_override("192.0.2.10")
                    .expect("host"),
                false,
            ),
        ] {
            assert_eq!(
                existing.matches_requested_intent(&request, &compose(&request)),
                expected,
                "{:?} {:?}",
                request.site_node_host(),
                request.site_node_host_override()
            );
        }
    }

    #[test]
    fn numeric_and_boolean_looking_names_round_trip_through_site_yaml() {
        for name in ["12345", "true"] {
            let yaml = compose_with_default(&simulated_request(), Some(name))
                .site_yaml()
                .expect("site YAML");
            let site: Site = yaml::from_str(&yaml).expect("site YAML loads");

            assert_eq!(site.nodes[0].display_name, name);
        }
    }

    fn simulated_request() -> InitialSiteRequest {
        InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
    }

    fn compose(request: &InitialSiteRequest) -> InitialSiteInputs {
        compose_with_default(request, None)
    }

    fn compose_with_default(
        request: &InitialSiteRequest,
        default_name: Option<&str>,
    ) -> InitialSiteInputs {
        InitialSiteInputs::compose(
            request,
            identities_with_default(request, default_name),
            &test_probe_report(false, None, std::iter::empty::<&str>()),
        )
        .expect("valid initial inputs")
    }

    fn identities(request: &InitialSiteRequest) -> crate::initial::request::InitialSiteIdentities {
        identities_with_default(request, None)
    }

    fn identities_with_default(
        request: &InitialSiteRequest,
        default_name: Option<&str>,
    ) -> crate::initial::request::InitialSiteIdentities {
        request
            .resolve_identities(&mut FixedIdentityGenerator {
                values: VecDeque::from([1, 2, 3, 4]),
                node_name: default_name.map(str::to_owned),
            })
            .expect("identities")
    }

    /// Each target's ID, name, platform, devices and runtime kind, in site order.
    fn target_details(
        inputs: &InitialSiteInputs,
    ) -> Vec<(
        String,
        &str,
        dotcore_domain::ExecutionPlatform,
        Vec<String>,
        &'static str,
    )> {
        inputs
            .site()
            .execution_targets
            .iter()
            .map(|target| {
                let runtime = match &target.runtime_binding {
                    Some(execution_target::RuntimeBinding::ModelRuntime(binding)) => {
                        match binding.adapter {
                            Some(model_runtime_binding::Adapter::SimulatedRuntime(_)) => {
                                "simulated"
                            }
                            _ => "other",
                        }
                    }
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) => {
                        match binding.adapter {
                            Some(
                                dotcore_proto::dotcore::site::v1::application_runtime_binding::Adapter::DockerRuntime(_),
                            ) => "docker",
                            _ => "other",
                        }
                    }
                    None => "other",
                };
                (
                    target.execution_target_id.clone(),
                    target.display_name.as_str(),
                    dotcore_domain::ExecutionPlatform::try_from(
                        target.platform.as_ref().expect("target platform"),
                    )
                    .expect("supported platform"),
                    target.accelerator_device_ids.clone(),
                    runtime,
                )
            })
            .collect()
    }

    /// Each model target's name and devices, in site order.
    /// Each target node `node_id` declares on `machine`: its ID, name, platform, devices and
    /// runtime kind, in site order.
    fn described_details(
        node_id: &str,
        machine: &MachineDescription,
    ) -> Vec<(
        String,
        String,
        dotcore_domain::ExecutionPlatform,
        Vec<String>,
        &'static str,
    )> {
        super::described_execution_targets(node_id, machine)
            .expect("described targets")
            .into_iter()
            .map(|target| {
                let runtime = match &target.runtime_binding {
                    Some(execution_target::RuntimeBinding::ModelRuntime(binding)) => {
                        match binding.adapter {
                            Some(model_runtime_binding::Adapter::NativeRuntime(_)) => "native",
                            Some(model_runtime_binding::Adapter::DockerRuntime(_)) => "docker",
                            _ => "other",
                        }
                    }
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) => {
                        match binding.adapter {
                            Some(
                                dotcore_proto::dotcore::site::v1::application_runtime_binding::Adapter::DockerRuntime(_),
                            ) => "docker",
                            _ => "other",
                        }
                    }
                    None => "other",
                };
                assert_eq!(target.site_node_id, node_id);
                (
                    target.execution_target_id,
                    target.display_name,
                    dotcore_domain::ExecutionPlatform::try_from(
                        target.platform.as_ref().expect("target platform"),
                    )
                    .expect("supported platform"),
                    target.accelerator_device_ids,
                    runtime,
                )
            })
            .collect()
    }

    /// Two dedicated RTX 5090 GPUs on Linux amd64.
    const TWO_NVIDIA_GPUS: &str = r#"
platform: linux/amd64
cpuCount: 16
hostMemory: { totalBytes: 68719476736, idleUsedBytes: 4294967296 }
gpus:
  - id: GPU-fixture-0
    vendor: nvidia
    model: NVIDIA GeForce RTX 5090
    driverVersion: "580.173.02"
    memory: { architecture: dedicated, totalBytes: 34190917632, reservedBytes: 536870912 }
  - id: GPU-fixture-1
    vendor: nvidia
    model: NVIDIA GeForce RTX 5090
    driverVersion: "580.173.02"
    memory: { architecture: dedicated, totalBytes: 34190917632, reservedBytes: 536870912 }
"#;

    /// A Linux arm64 machine without a GPU.
    const NO_GPU: &str = r#"
platform: linux/arm64
cpuCount: 4
hostMemory: { totalBytes: 8589934592, idleUsedBytes: 1073741824 }
"#;

    fn model_target_devices(inputs: &InitialSiteInputs) -> Vec<(&str, Vec<String>)> {
        inputs
            .site()
            .execution_targets
            .iter()
            .filter(|target| {
                matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ModelRuntime(_))
                )
            })
            .map(|target| {
                (
                    target.display_name.as_str(),
                    target.accelerator_device_ids.clone(),
                )
            })
            .collect()
    }

    fn target_names(inputs: &InitialSiteInputs) -> Vec<&str> {
        inputs
            .site()
            .execution_targets
            .iter()
            .map(|target| target.display_name.as_str())
            .collect()
    }

    fn nvidia_facts() -> Vec<AcceleratorFacts> {
        nvidia_facts_with_count(1)
    }

    fn nvidia_facts_with_count(count: u32) -> Vec<AcceleratorFacts> {
        (0..count)
            .map(|index| AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: RTX_5090.to_owned(),
                driver_or_runtime_version: "580.173.02".to_owned(),
                device_id: format!("GPU-fixture-{index}"),
                vendor_index: Some(index),
                ..Default::default()
            })
            .collect()
    }

    /// Lists each target's ID, devices and display name in site order.
    fn target_summaries(inputs: &InitialSiteInputs) -> Vec<(String, Vec<String>, String)> {
        inputs
            .site()
            .execution_targets
            .iter()
            .map(|target| {
                (
                    target.execution_target_id.clone(),
                    target.accelerator_device_ids.clone(),
                    target.display_name.clone(),
                )
            })
            .collect()
    }

    struct FixedIdentityGenerator {
        values: VecDeque<u64>,
        node_name: Option<String>,
    }

    impl InitialSiteIdentityGenerator for FixedIdentityGenerator {
        fn generate(&mut self, _kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
            self.values
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("missing identity"))
        }

        fn node_display_name(&self) -> Option<String> {
            self.node_name.clone()
        }
    }
}
