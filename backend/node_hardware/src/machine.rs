//! Declared machines: a machine's hardware described as data instead of read from the host.
//!
//! A description states a machine's platform, host memory and GPUs, and renders the hardware
//! snapshot a reading of that machine would give, with the memory its running workloads take.
//! Named machines and named GPUs are compiled into the binary, one YAML file each: simulated
//! hardware puts any set of named GPUs on one simulated host, or takes a whole named machine, so
//! any node or test can run hardware it doesn't have. A description converts to and from the
//! stored form a node keeps, and every source is checked with the same rules.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::SystemTime,
};

use dotcore_domain::{
    Accelerator, ExecutionPlatform, ExecutionPlatformError, clock::SystemClock,
    hardware_observation::HardwareSnapshotSource,
};
use dotcore_proto::{
    dotcore::{
        node::v1::{
            DedicatedGpuMemory, GpuDescription as StoredGpu, MachineDescription as StoredMachine,
            gpu_description,
        },
        observation::v1::{
            AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
            AcceleratorObservation, AcceleratorVendor, HardwareSnapshot, HostFacts,
            HostObservation, accelerator_observation, host_observation,
        },
    },
    google::protobuf::Empty,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::hardware::{SystemHardwareSnapshotCollector, timestamp_from};

/// The named machines compiled into the binary: each name with its YAML description.
const NAMED_MACHINES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/named_machines.rs"));

/// The named GPUs compiled into the binary: each name with its YAML description.
const NAMED_GPUS: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/named_gpus.rs"));

/// The host simulated GPUs are put on, as YAML.
const SIMULATED_HOST: &str = include_str!("../simulated/host.yaml");

/// The name a description of the local host is given. It never derives from the host's own name,
/// so the description can be shared.
const LOCAL_MACHINE_NAME: &str = "local-machine";

/// One machine's hardware: its platform, host memory and GPUs. It renders the hardware snapshot
/// the machine reports while some workloads run on it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineDescription {
    #[serde(skip)]
    name: String,

    #[serde(
        deserialize_with = "deserialize_platform",
        serialize_with = "serialize_platform"
    )]
    platform: ExecutionPlatform,

    cpu_count: u32,

    host_memory: HostMemoryDescription,

    #[serde(default)]
    gpus: Vec<GpuDescription>,
}

/// Memory one running workload takes on a declared machine, from its resolved estimate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MachineWorkload {
    /// Bytes on each GPU, by GPU ID; a GPU with unified memory takes them from host memory.
    pub gpu_bytes: BTreeMap<String, u64>,

    /// Bytes taken from host memory outside any GPU.
    pub host_bytes: u64,
}

/// One hardware choice simulated inference offers: a named GPU, put on the simulated host with any
/// other chosen GPUs, or a named machine, which stands alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatedChoice {
    /// Name the choice is given by.
    pub name: &'static str,

    /// One-line summary for an operator picking hardware.
    pub summary: String,

    /// Whether the choice is a whole machine, which can't be combined with other choices.
    pub whole_machine: bool,
}

/// A machine that can't be found, read, or run a workload.
#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("no named machine {name:?}; known machines: {known}")]
    UnknownMachine { name: String, known: String },

    #[error("machine description {machine:?} is malformed")]
    Malformed {
        machine: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error("machine description {machine:?} declares GPU {gpu_id:?} more than once")]
    DuplicateGpu { machine: String, gpu_id: String },

    #[error("a workload uses GPU {gpu_id:?}, which machine {machine:?} doesn't have")]
    UnknownGpu { machine: String, gpu_id: String },

    #[error("machine description {machine:?} has GPU {gpu_id:?}, which {reason}")]
    InvalidGpu {
        machine: String,
        gpu_id: String,
        reason: &'static str,
    },

    #[error("GPU description {gpu:?} is malformed")]
    MalformedGpu {
        gpu: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error("the simulated host description is malformed")]
    MalformedHost(#[source] serde_yaml_ng::Error),

    #[error("the simulated host declares no driver for the GPU vendor of {gpu:?}")]
    NoHostDriver { gpu: String },

    #[error("no simulated hardware is chosen")]
    NoSimulatedChoice,

    #[error("no simulated hardware {name:?}; known: {known}")]
    UnknownSimulatedChoice { name: String, known: String },

    #[error("simulated hardware {name:?} is chosen more than once")]
    RepeatedSimulatedChoice { name: String },

    #[error("{name:?} is a whole machine and can't be combined with other simulated hardware")]
    MachineNotAlone { name: String },

    #[error("simulated hardware can't run on CPU architecture {architecture:?}")]
    SimulatedArchitecture {
        architecture: String,
        #[source]
        source: ExecutionPlatformError,
    },

    #[error("this host's platform is not supported")]
    LocalPlatform(#[source] ExecutionPlatformError),

    #[error("this host's hardware facts are unavailable, so it can't be described")]
    LocalHostUnavailable,

    #[error("the host's GPU {gpu_id:?} can't be described: {reason}")]
    LocalGpu {
        gpu_id: String,
        reason: &'static str,
    },

    #[error("machine description can't be written as YAML")]
    Serialize(#[source] serde_yaml_ng::Error),

    #[error("stored machine description {machine:?} has an invalid platform")]
    StoredPlatform {
        machine: String,
        #[source]
        source: ExecutionPlatformError,
    },

    #[error("stored machine description {machine:?} has no valid {field}")]
    StoredField {
        machine: String,
        field: &'static str,
    },
}

impl MachineDescription {
    /// Describes this host from a fresh reading of its hardware, under a fixed name that is not
    /// the host's name.
    pub fn from_local_host() -> Result<Self, MachineError> {
        // Rust reports macOS as "macos"; platforms call it darwin.
        let operating_system = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let platform = ExecutionPlatform::normalize(operating_system, std::env::consts::ARCH)
            .map_err(MachineError::LocalPlatform)?;
        let snapshot =
            SystemHardwareSnapshotCollector::with_platform_accelerators(Arc::new(SystemClock))
                .collect_snapshot();
        Self::from_snapshot(platform, &snapshot)
    }

    /// Describes a host from a collected hardware snapshot and the platform it runs. The host's
    /// used memory at collection is the idle figure, as is each dedicated GPU's used memory; the
    /// snapshot's host name and filesystems are dropped. A vendor whose facts are unavailable
    /// contributes no GPUs.
    pub fn from_snapshot(
        platform: ExecutionPlatform,
        snapshot: &HardwareSnapshot,
    ) -> Result<Self, MachineError> {
        let Some(host_observation::Result::Facts(host)) = snapshot
            .host
            .as_ref()
            .and_then(|observation| observation.result.as_ref())
        else {
            return Err(MachineError::LocalHostUnavailable);
        };

        let gpus = snapshot
            .accelerators
            .iter()
            .filter_map(|observation| match observation.result.as_ref() {
                Some(accelerator_observation::Result::Facts(set)) => Some(set.facts.iter()),
                _ => None,
            })
            .flatten()
            .map(GpuDescription::from_facts)
            .collect::<Result<_, MachineError>>()?;

        let machine = Self {
            name: LOCAL_MACHINE_NAME.to_owned(),
            platform,
            cpu_count: host.cpu_count,
            host_memory: HostMemoryDescription {
                total_bytes: host.total_memory_bytes,
                idle_used_bytes: host
                    .total_memory_bytes
                    .saturating_sub(host.available_memory_bytes),
            },
            gpus,
        };
        machine.validate()?;
        Ok(machine)
    }

    /// Writes the description as the YAML form named machines are written in, which
    /// `from_yaml` reads back. The name is not part of it.
    pub fn to_yaml(&self) -> Result<String, MachineError> {
        serde_yaml_ng::to_string(self).map_err(MachineError::Serialize)
    }

    /// Returns the name of every machine compiled into the binary, in a stable order.
    pub fn named_machines() -> impl Iterator<Item = &'static str> {
        NAMED_MACHINES.iter().map(|(name, _)| *name)
    }

    /// Returns the named machine compiled into the binary.
    pub fn named(name: &str) -> Result<Self, MachineError> {
        let (_, yaml) = NAMED_MACHINES
            .iter()
            .find(|(known, _)| *known == name)
            .ok_or_else(|| MachineError::UnknownMachine {
                name: name.to_owned(),
                known: NAMED_MACHINES
                    .iter()
                    .map(|(known, _)| *known)
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
        Self::from_yaml(name, yaml)
    }

    /// Reads a machine description from its YAML form, under the given name.
    pub fn from_yaml(name: &str, yaml: &str) -> Result<Self, MachineError> {
        let mut machine: Self =
            serde_yaml_ng::from_str(yaml).map_err(|source| MachineError::Malformed {
                machine: name.to_owned(),
                source,
            })?;
        machine.name = name.to_owned();
        machine.validate()?;
        Ok(machine)
    }

    /// Returns every hardware choice simulated inference offers: the named GPUs, then the named
    /// machines, each with a summary.
    pub fn simulated_choices() -> Vec<SimulatedChoice> {
        let gpus = NAMED_GPUS.iter().map(|(name, yaml)| SimulatedChoice {
            name,
            summary: named_gpu(name, yaml).map_or_else(|_| String::new(), |gpu| gpu.summary()),
            whole_machine: false,
        });
        let machines = NAMED_MACHINES.iter().map(|(name, yaml)| SimulatedChoice {
            name,
            summary: Self::from_yaml(name, yaml)
                .map_or_else(|_| String::new(), |machine| machine.summary()),
            whole_machine: true,
        });
        gpus.chain(machines).collect()
    }

    /// Resolves simulated hardware choices, in order, for a node on CPU `architecture`: named GPUs
    /// go on the simulated host, running Linux on that architecture, with the host's driver for
    /// each vendor; a named machine stands alone and keeps its own platform. The choices must be
    /// distinct and known.
    pub fn simulated(choices: &[String], architecture: &str) -> Result<Self, MachineError> {
        let [first, rest @ ..] = choices else {
            return Err(MachineError::NoSimulatedChoice);
        };
        let mut seen = BTreeSet::new();
        if let Some(name) = choices.iter().find(|name| !seen.insert(name.as_str())) {
            return Err(MachineError::RepeatedSimulatedChoice { name: name.clone() });
        }
        for name in choices {
            let machine = NAMED_MACHINES.iter().any(|(known, _)| known == name);
            let gpu = NAMED_GPUS.iter().any(|(known, _)| known == name);
            if machine && choices.len() > 1 {
                return Err(MachineError::MachineNotAlone { name: name.clone() });
            }
            if !machine && !gpu {
                return Err(MachineError::UnknownSimulatedChoice {
                    name: name.clone(),
                    known: Self::simulated_choices()
                        .iter()
                        .map(|choice| choice.name)
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        }
        if rest.is_empty() && NAMED_MACHINES.iter().any(|(known, _)| known == first) {
            return Self::named(first);
        }

        let platform = ExecutionPlatform::normalize("linux", architecture).map_err(|source| {
            MachineError::SimulatedArchitecture {
                architecture: architecture.to_owned(),
                source,
            }
        })?;
        let host: SimulatedHost =
            serde_yaml_ng::from_str(SIMULATED_HOST).map_err(MachineError::MalformedHost)?;
        let gpus = choices
            .iter()
            .map(|name| {
                let (_, yaml) = NAMED_GPUS
                    .iter()
                    .find(|(known, _)| known == name)
                    .expect("checked above");
                let gpu = named_gpu(name, yaml)?;
                let driver_version = host
                    .drivers
                    .get(vendor_name(gpu.vendor))
                    .cloned()
                    .ok_or_else(|| MachineError::NoHostDriver { gpu: name.clone() })?;
                Ok(GpuDescription {
                    id: gpu.id,
                    vendor: gpu.vendor,
                    model: gpu.model,
                    driver_version,
                    memory: gpu.memory,
                    working_set_limit_bytes: None,
                })
            })
            .collect::<Result<_, MachineError>>()?;

        let machine = Self {
            name: format!("simulated-{}", choices.join("-")),
            platform,
            cpu_count: host.cpu_count,
            host_memory: host.host_memory,
            gpus,
        };
        machine.validate()?;
        Ok(machine)
    }

    /// Reads a description from the form a node stores, checked like a YAML description.
    pub fn from_stored(stored: &StoredMachine) -> Result<Self, MachineError> {
        let field = |field| MachineError::StoredField {
            machine: stored.name.clone(),
            field,
        };
        let platform = stored.platform.as_ref().ok_or_else(|| field("platform"))?;
        let platform = ExecutionPlatform::try_from(platform).map_err(|source| {
            MachineError::StoredPlatform {
                machine: stored.name.clone(),
                source,
            }
        })?;
        let gpus = stored
            .gpus
            .iter()
            .map(|gpu| {
                let vendor =
                    Accelerator::from_proto(gpu.vendor).ok_or_else(|| field("GPU vendor"))?;
                let memory = match gpu.memory.as_ref().ok_or_else(|| field("GPU memory"))? {
                    gpu_description::Memory::Dedicated(memory) => GpuMemory::Dedicated {
                        total_bytes: memory.total_bytes,
                        reserved_bytes: memory.reserved_bytes,
                    },
                    gpu_description::Memory::Unified(_) => GpuMemory::Unified,
                };
                Ok(GpuDescription {
                    id: gpu.id.clone(),
                    vendor,
                    model: gpu.model.clone(),
                    driver_version: gpu.driver_version.clone(),
                    memory,
                    working_set_limit_bytes: gpu.working_set_limit_bytes,
                })
            })
            .collect::<Result<_, MachineError>>()?;

        let machine = Self {
            name: stored.name.clone(),
            platform,
            cpu_count: stored.cpu_count,
            host_memory: HostMemoryDescription {
                total_bytes: stored.host_memory_total_bytes,
                idle_used_bytes: stored.host_memory_idle_used_bytes,
            },
            gpus,
        };
        machine.validate()?;
        Ok(machine)
    }

    /// Returns the form a node stores this description in.
    pub fn to_stored(&self) -> StoredMachine {
        StoredMachine {
            name: self.name.clone(),
            platform: Some(self.platform.to_oci_platform()),
            cpu_count: self.cpu_count,
            host_memory_total_bytes: self.host_memory.total_bytes,
            host_memory_idle_used_bytes: self.host_memory.idle_used_bytes,
            gpus: self
                .gpus
                .iter()
                .map(|gpu| StoredGpu {
                    id: gpu.id.clone(),
                    vendor: proto_vendor(gpu.vendor) as i32,
                    model: gpu.model.clone(),
                    driver_version: gpu.driver_version.clone(),
                    memory: Some(match gpu.memory {
                        GpuMemory::Dedicated {
                            total_bytes,
                            reserved_bytes,
                        } => gpu_description::Memory::Dedicated(DedicatedGpuMemory {
                            total_bytes,
                            reserved_bytes,
                        }),
                        GpuMemory::Unified => gpu_description::Memory::Unified(Empty {}),
                    }),
                    working_set_limit_bytes: gpu.working_set_limit_bytes,
                })
                .collect(),
        }
    }

    /// Returns the machine's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the platform the machine runs: its operating system and CPU architecture.
    pub fn platform(&self) -> &ExecutionPlatform {
        &self.platform
    }

    /// Returns the ID of every GPU of the machine, in the order the machine lists them.
    pub fn gpu_ids(&self) -> impl Iterator<Item = &str> {
        self.gpus.iter().map(|gpu| gpu.id.as_str())
    }

    /// Returns the vendor of the machine's GPU `gpu_id`, if the machine has it.
    pub fn gpu_vendor(&self, gpu_id: &str) -> Option<Accelerator> {
        self.gpu(gpu_id).ok().map(|gpu| gpu.vendor)
    }

    /// Returns the facts every GPU of the machine reports with no workload running: the same
    /// model, identity and enumeration index a hardware snapshot carries.
    pub fn accelerator_facts(&self) -> Vec<AcceleratorFacts> {
        self.accelerator_observations(&BTreeMap::new())
            .into_iter()
            .filter_map(|observation| match observation.result {
                Some(accelerator_observation::Result::Facts(set)) => Some(set.facts),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Checks that a workload can run on the machine: every GPU it uses is one of the machine's.
    pub fn check_workload(&self, workload: &MachineWorkload) -> Result<(), MachineError> {
        workload
            .gpu_bytes
            .keys()
            .try_for_each(|gpu_id| self.gpu(gpu_id).map(|_| ()))
    }

    /// Renders the hardware snapshot this machine reports at `observed_at` while `workloads` run
    /// on it. Each workload's bytes count as used on its GPUs, or on host memory for unified GPUs
    /// and host bytes; usage is never capped, so an overcommitted machine shows no free memory.
    pub fn render(
        &self,
        workloads: &[MachineWorkload],
        observed_at: SystemTime,
    ) -> Result<HardwareSnapshot, MachineError> {
        let mut host_used = self.host_memory.idle_used_bytes;
        let mut gpu_used: BTreeMap<&str, u64> = BTreeMap::new();
        for workload in workloads {
            host_used = host_used.saturating_add(workload.host_bytes);
            for (gpu_id, bytes) in &workload.gpu_bytes {
                let gpu = self.gpu(gpu_id)?;
                match gpu.memory {
                    GpuMemory::Dedicated { .. } => {
                        let used = gpu_used.entry(gpu.id.as_str()).or_default();
                        *used = used.saturating_add(*bytes);
                    }
                    GpuMemory::Unified => host_used = host_used.saturating_add(*bytes),
                }
            }
        }

        let host = HostFacts {
            host_name: self.name.clone(),
            operating_system_or_kernel: self.platform.to_string(),
            cpu_count: self.cpu_count,
            total_memory_bytes: self.host_memory.total_bytes,
            available_memory_bytes: self.host_memory.total_bytes.saturating_sub(host_used),
            cached_memory_bytes: None,
            filesystems: Vec::new(),
        };
        Ok(HardwareSnapshot {
            observed_at: Some(timestamp_from(observed_at)),
            host: Some(HostObservation {
                result: Some(host_observation::Result::Facts(host)),
            }),
            accelerators: self.accelerator_observations(&gpu_used),
        })
    }

    /// Checks the rules every description follows: distinct GPU IDs, NVIDIA GPUs on Linux, and
    /// Apple GPUs on darwin with unified memory.
    fn validate(&self) -> Result<(), MachineError> {
        let mut seen = BTreeSet::new();
        for gpu in &self.gpus {
            if !seen.insert(&gpu.id) {
                return Err(MachineError::DuplicateGpu {
                    machine: self.name.clone(),
                    gpu_id: gpu.id.clone(),
                });
            }
            let reason = match gpu.vendor {
                Accelerator::Nvidia if !self.platform.is_linux() => Some("needs a Linux platform"),
                Accelerator::Apple if self.platform.is_linux() => Some("needs a darwin platform"),
                Accelerator::Apple if gpu.memory != GpuMemory::Unified => {
                    Some("is an Apple GPU, whose memory is unified")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                return Err(MachineError::InvalidGpu {
                    machine: self.name.clone(),
                    gpu_id: gpu.id.clone(),
                    reason,
                });
            }
        }
        Ok(())
    }

    /// A one-line summary of a whole machine: its GPUs and host memory.
    fn summary(&self) -> String {
        let gpus = self
            .gpus
            .iter()
            .map(|gpu| gpu.model.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "whole machine: {gpus}, {} host memory, {}",
            gibibytes(self.host_memory.total_bytes),
            self.platform
        )
    }

    fn gpu(&self, gpu_id: &str) -> Result<&GpuDescription, MachineError> {
        self.gpus
            .iter()
            .find(|gpu| gpu.id == gpu_id)
            .ok_or_else(|| MachineError::UnknownGpu {
                machine: self.name.clone(),
                gpu_id: gpu_id.to_owned(),
            })
    }

    /// One observation per vendor, in the order the machine lists them; each GPU's vendor index
    /// is its position among the vendor's GPUs, as the vendor would enumerate them.
    fn accelerator_observations(
        &self,
        gpu_used: &BTreeMap<&str, u64>,
    ) -> Vec<AcceleratorObservation> {
        let mut observations: Vec<(Accelerator, Vec<AcceleratorFacts>)> = Vec::new();
        for gpu in &self.gpus {
            let facts = match observations
                .iter_mut()
                .find(|(vendor, _)| *vendor == gpu.vendor)
            {
                Some((_, facts)) => facts,
                None => {
                    observations.push((gpu.vendor, Vec::new()));
                    &mut observations.last_mut().expect("just pushed").1
                }
            };
            let vendor_index = u32::try_from(facts.len()).expect("GPU count fits u32");
            facts.push(gpu.facts(vendor_index, gpu_used.get(gpu.id.as_str()).copied()));
        }

        observations
            .into_iter()
            .map(|(vendor, facts)| AcceleratorObservation {
                vendor: proto_vendor(vendor) as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet { facts },
                )),
            })
            .collect()
    }
}

/// A machine's host memory: its total and what the system uses with no workload running.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostMemoryDescription {
    total_bytes: u64,
    idle_used_bytes: u64,
}

/// One GPU of a machine, under the ID its vendor reports.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GpuDescription {
    id: String,

    #[serde(
        deserialize_with = "deserialize_vendor",
        serialize_with = "serialize_vendor"
    )]
    vendor: Accelerator,

    model: String,

    /// Empty for Apple GPUs, whose Metal runtime ships with the operating system.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    driver_version: String,

    memory: GpuMemory,

    #[serde(skip_serializing_if = "Option::is_none")]
    working_set_limit_bytes: Option<u64>,
}

impl GpuDescription {
    /// Reads one GPU from the facts a collector reported for it. A dedicated GPU's used memory
    /// is the memory its driver holds with no workload running.
    fn from_facts(facts: &AcceleratorFacts) -> Result<Self, MachineError> {
        let error = |reason| MachineError::LocalGpu {
            gpu_id: facts.device_id.clone(),
            reason,
        };
        let vendor = Accelerator::from_proto(facts.vendor)
            .ok_or_else(|| error("has a vendor Dotcore can't describe"))?;
        let memory = match facts.memory_architecture() {
            AcceleratorMemoryArchitecture::Dedicated => GpuMemory::Dedicated {
                total_bytes: facts
                    .total_memory_bytes
                    .ok_or_else(|| error("reports no total memory"))?,
                reserved_bytes: facts
                    .used_memory_bytes
                    .ok_or_else(|| error("reports no used memory"))?,
            },
            AcceleratorMemoryArchitecture::Unified => GpuMemory::Unified,
            AcceleratorMemoryArchitecture::Unspecified => {
                return Err(error("has an unknown memory architecture"));
            }
        };
        Ok(Self {
            id: facts.device_id.clone(),
            vendor,
            model: facts.model.clone(),
            driver_version: facts.driver_or_runtime_version.clone(),
            memory,
            working_set_limit_bytes: facts.working_set_limit_bytes,
        })
    }

    /// The facts this GPU reports with `workload_bytes` used by workloads on its dedicated memory.
    fn facts(&self, vendor_index: u32, workload_bytes: Option<u64>) -> AcceleratorFacts {
        let (architecture, total, used) = match self.memory {
            GpuMemory::Dedicated {
                total_bytes,
                reserved_bytes,
            } => (
                AcceleratorMemoryArchitecture::Dedicated,
                Some(total_bytes),
                // Driver-reserved memory counts as used, as the vendor reports it.
                Some(reserved_bytes.saturating_add(workload_bytes.unwrap_or_default())),
            ),
            // Unified GPUs report no memory of their own; host facts describe the pool.
            GpuMemory::Unified => (AcceleratorMemoryArchitecture::Unified, None, None),
        };
        AcceleratorFacts {
            vendor: proto_vendor(self.vendor) as i32,
            model: self.model.clone(),
            memory_architecture: architecture as i32,
            total_memory_bytes: total,
            driver_or_runtime_version: self.driver_version.clone(),
            device_id: self.id.clone(),
            vendor_index: Some(vendor_index),
            used_memory_bytes: used,
            working_set_limit_bytes: self.working_set_limit_bytes,
            supported_signals: Vec::new(),
        }
    }
}

/// How a GPU's memory relates to the host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "architecture", rename_all = "camelCase")]
enum GpuMemory {
    /// The GPU owns its memory; the driver holds part of it even with no workload running.
    #[serde(rename_all = "camelCase")]
    Dedicated {
        total_bytes: u64,
        reserved_bytes: u64,
    },

    /// The GPU allocates from the host memory.
    Unified,
}

/// One named GPU's file: a dedicated GPU without its driver, which the simulated host supplies.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NamedGpu {
    id: String,

    #[serde(deserialize_with = "deserialize_vendor")]
    vendor: Accelerator,

    model: String,

    memory: GpuMemory,
}

impl NamedGpu {
    /// A one-line summary: the model and its memory.
    fn summary(&self) -> String {
        match self.memory {
            GpuMemory::Dedicated { total_bytes, .. } => {
                format!("{}, {}", self.model, gibibytes(total_bytes))
            }
            GpuMemory::Unified => self.model.clone(),
        }
    }
}

/// The host simulated GPUs are put on: its CPUs, its memory and each GPU vendor's driver version.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SimulatedHost {
    cpu_count: u32,

    host_memory: HostMemoryDescription,

    drivers: BTreeMap<String, String>,
}

/// Reads one named GPU; a GPU sharing host memory belongs to a whole machine instead.
fn named_gpu(name: &str, yaml: &str) -> Result<NamedGpu, MachineError> {
    let gpu: NamedGpu =
        serde_yaml_ng::from_str(yaml).map_err(|source| MachineError::MalformedGpu {
            gpu: name.to_owned(),
            source,
        })?;
    if gpu.memory == GpuMemory::Unified {
        return Err(MachineError::InvalidGpu {
            machine: name.to_owned(),
            gpu_id: gpu.id,
            reason: "shares host memory, so it belongs to a whole machine",
        });
    }
    Ok(gpu)
}

/// Bytes as whole gibibytes, rounded to the nearest one.
fn gibibytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    format!("{} GiB", (bytes + GIB / 2) / GIB)
}

/// The vendor's name in hardware descriptions.
fn vendor_name(vendor: Accelerator) -> &'static str {
    match vendor {
        Accelerator::Nvidia => "nvidia",
        Accelerator::Apple => "apple",
    }
}

fn proto_vendor(vendor: Accelerator) -> AcceleratorVendor {
    match vendor {
        Accelerator::Nvidia => AcceleratorVendor::Nvidia,
        Accelerator::Apple => AcceleratorVendor::Apple,
    }
}

fn deserialize_platform<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ExecutionPlatform, D::Error> {
    let value = String::deserialize(deserializer)?;
    let (operating_system, architecture) = value
        .split_once('/')
        .ok_or_else(|| D::Error::custom("expected a platform such as linux/amd64"))?;
    ExecutionPlatform::normalize(operating_system, architecture).map_err(D::Error::custom)
}

fn serialize_platform<S: Serializer>(
    platform: &ExecutionPlatform,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(platform)
}

fn serialize_vendor<S: Serializer>(vendor: &Accelerator, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(vendor_name(*vendor))
}

fn deserialize_vendor<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Accelerator, D::Error> {
    let value = String::deserialize(deserializer)?;
    Accelerator::from_catalog_name(&value).ok_or_else(|| {
        D::Error::custom(format!(
            "unknown GPU vendor {value:?}; expected nvidia or apple"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use dotcore_proto::dotcore::observation::v1::FilesystemFacts;

    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn every_named_machine_loads_under_its_file_name() {
        let names: Vec<_> = NAMED_MACHINES.iter().map(|(name, _)| *name).collect();

        assert_eq!(names, ["gb10", "mac-64g"]);
        for name in names {
            let machine = MachineDescription::named(name).expect("named machine loads");
            assert_eq!(machine.name(), name);
        }
    }

    #[test]
    fn a_simulated_rtx_5090_renders_one_dedicated_gpu_with_reserved_memory() {
        let snapshot = render("rtx-5090", &[]);

        assert_eq!(snapshot.observed_at, Some(timestamp_from(observed_at())));
        assert_eq!(
            host(&snapshot),
            HostFacts {
                host_name: "simulated-rtx-5090".to_owned(),
                operating_system_or_kernel: "linux/amd64".to_owned(),
                cpu_count: 16,
                total_memory_bytes: 64 * GIB,
                available_memory_bytes: 60 * GIB,
                cached_memory_bytes: None,
                filesystems: Vec::new(),
            }
        );
        assert_eq!(
            snapshot.accelerators,
            vec![vendor_observation(
                AcceleratorVendor::Nvidia,
                vec![AcceleratorFacts {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    model: "NVIDIA GeForce RTX 5090".to_owned(),
                    memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                    total_memory_bytes: Some(32_607 * MIB),
                    driver_or_runtime_version: "595.71.05".to_owned(),
                    device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
                    vendor_index: Some(0),
                    used_memory_bytes: Some(497 * MIB),
                    working_set_limit_bytes: None,
                    supported_signals: Vec::new(),
                }]
            )]
        );
    }

    #[test]
    fn gb10_renders_a_unified_gpu_sharing_host_memory() {
        let snapshot = render("gb10", &[]);

        let host = host(&snapshot);
        assert_eq!(host.operating_system_or_kernel, "linux/arm64");
        assert_eq!(host.cpu_count, 20);
        assert_eq!(host.total_memory_bytes, 130_596_048_896);
        assert_eq!(host.available_memory_bytes, 130_596_048_896 - 7_938_146_304);
        assert_eq!(
            snapshot.accelerators,
            vec![vendor_observation(
                AcceleratorVendor::Nvidia,
                vec![AcceleratorFacts {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    model: "NVIDIA GB10".to_owned(),
                    memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
                    total_memory_bytes: None,
                    driver_or_runtime_version: "580.173.02".to_owned(),
                    device_id: "GPU-00000000-0000-0000-0000-00000000gb10".to_owned(),
                    vendor_index: Some(0),
                    used_memory_bytes: None,
                    working_set_limit_bytes: None,
                    supported_signals: Vec::new(),
                }]
            )]
        );
    }

    #[test]
    fn mac_64g_renders_a_unified_apple_gpu_with_its_working_set_limit() {
        let snapshot = render("mac-64g", &[]);

        let host = host(&snapshot);
        assert_eq!(host.operating_system_or_kernel, "darwin/arm64");
        assert_eq!(host.total_memory_bytes, 64 * GIB);
        assert_eq!(host.available_memory_bytes, 56 * GIB);
        assert_eq!(
            snapshot.accelerators,
            vec![vendor_observation(
                AcceleratorVendor::Apple,
                vec![AcceleratorFacts {
                    vendor: AcceleratorVendor::Apple as i32,
                    model: "Apple M4 Max".to_owned(),
                    memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
                    total_memory_bytes: None,
                    driver_or_runtime_version: String::new(),
                    device_id: "apple-gpu-0".to_owned(),
                    vendor_index: Some(0),
                    used_memory_bytes: None,
                    working_set_limit_bytes: Some(48 * GIB),
                    supported_signals: Vec::new(),
                }]
            )]
        );
    }

    #[test]
    fn simulated_gpus_go_on_the_simulated_host_in_the_order_chosen() {
        for (architecture, platform) in [
            ("x86_64", ExecutionPlatform::linux_amd64()),
            ("aarch64", ExecutionPlatform::linux_arm64()),
        ] {
            let machine =
                MachineDescription::simulated(&choices(&["rtx-4090", "rtx-4080"]), architecture)
                    .expect("simulated machine");
            let snapshot = machine.render(&[], observed_at()).expect("snapshot");

            assert_eq!(machine.name(), "simulated-rtx-4090-rtx-4080");
            assert_eq!(machine.platform(), &platform);
            assert_eq!(
                host(&snapshot),
                HostFacts {
                    host_name: "simulated-rtx-4090-rtx-4080".to_owned(),
                    operating_system_or_kernel: platform.to_string(),
                    cpu_count: 16,
                    total_memory_bytes: 64 * GIB,
                    available_memory_bytes: 60 * GIB,
                    cached_memory_bytes: None,
                    filesystems: Vec::new(),
                }
            );
            assert_eq!(
                dedicated_memory(&snapshot),
                vec![
                    (
                        "GPU-00000000-0000-0000-0000-000000004090",
                        0,
                        24 * GIB,
                        497 * MIB
                    ),
                    (
                        "GPU-00000000-0000-0000-0000-000000004080",
                        1,
                        16 * GIB,
                        497 * MIB
                    ),
                ]
            );
            assert_eq!(drivers(&snapshot), ["595.71.05", "595.71.05"]);
        }
    }

    #[test]
    fn one_simulated_gpu_reports_like_the_gpu_it_describes() {
        let machine = MachineDescription::simulated(&choices(&["rtx-5090"]), "amd64")
            .expect("simulated machine");
        let snapshot = machine.render(&[], observed_at()).expect("snapshot");

        assert_eq!(machine.name(), "simulated-rtx-5090");
        assert_eq!(
            dedicated_memory(&snapshot),
            vec![(
                "GPU-00000000-0000-0000-0000-000000005090",
                0,
                32_607 * MIB,
                497 * MIB
            )]
        );
        assert_eq!(
            MachineDescription::simulated(&choices(&["rtx-pro-6000"]), "amd64")
                .expect("simulated machine")
                .render(&[], observed_at())
                .map(|snapshot| dedicated_memory(&snapshot)
                    .into_iter()
                    .map(|(_, _, total, _)| total)
                    .collect::<Vec<_>>())
                .expect("snapshot"),
            [97_887 * MIB]
        );
    }

    #[test]
    fn a_whole_machine_chosen_alone_is_the_named_machine() {
        for name in ["gb10", "mac-64g"] {
            assert_eq!(
                MachineDescription::simulated(&choices(&[name]), "x86_64").expect(name),
                MachineDescription::named(name).expect(name),
            );
        }
    }

    #[test]
    fn invalid_simulated_choices_are_errors() {
        let cases: [(&[&str], &str, &str); 5] = [
            (&[], "amd64", "no simulated hardware is chosen"),
            (
                &["rtx-5090", "rtx-5090"],
                "amd64",
                "simulated hardware \"rtx-5090\" is chosen more than once",
            ),
            (
                &["gb10", "rtx-5090"],
                "amd64",
                "\"gb10\" is a whole machine and can't be combined with other simulated hardware",
            ),
            (
                &["rtx-9090"],
                "amd64",
                "no simulated hardware \"rtx-9090\"; known: rtx-4080, rtx-4090, rtx-5090, \
                 rtx-pro-6000, gb10, mac-64g",
            ),
            (
                &["rtx-5090"],
                "riscv",
                "simulated hardware can't run on CPU architecture \"riscv\"",
            ),
        ];

        for (names, architecture, message) in cases {
            let error =
                MachineDescription::simulated(&choices(names), architecture).expect_err(message);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn simulated_choices_list_gpus_then_whole_machines_with_a_summary() {
        let summaries: Vec<_> = MachineDescription::simulated_choices()
            .into_iter()
            .map(|choice| (choice.name, choice.summary, choice.whole_machine))
            .collect();

        assert_eq!(
            summaries,
            [
                (
                    "rtx-4080",
                    "NVIDIA GeForce RTX 4080, 16 GiB".to_owned(),
                    false
                ),
                (
                    "rtx-4090",
                    "NVIDIA GeForce RTX 4090, 24 GiB".to_owned(),
                    false
                ),
                (
                    "rtx-5090",
                    "NVIDIA GeForce RTX 5090, 32 GiB".to_owned(),
                    false
                ),
                (
                    "rtx-pro-6000",
                    "NVIDIA RTX PRO 6000 Blackwell Workstation Edition, 96 GiB".to_owned(),
                    false
                ),
                (
                    "gb10",
                    "whole machine: NVIDIA GB10, 122 GiB host memory, linux/arm64".to_owned(),
                    true
                ),
                (
                    "mac-64g",
                    "whole machine: Apple M4 Max, 64 GiB host memory, darwin/arm64".to_owned(),
                    true
                ),
            ]
        );
    }

    #[test]
    fn a_description_survives_its_stored_form() {
        let simulated = MachineDescription::simulated(&choices(&["rtx-4090", "rtx-4080"]), "arm64")
            .expect("simulated machine");
        for machine in [
            simulated,
            MachineDescription::named("gb10").expect("named machine"),
            MachineDescription::named("mac-64g").expect("named machine"),
        ] {
            assert_eq!(
                MachineDescription::from_stored(&machine.to_stored()).expect("stored form"),
                machine
            );
        }
    }

    #[test]
    fn an_invalid_stored_form_is_an_error() {
        let valid = MachineDescription::simulated(&choices(&["rtx-5090", "rtx-4090"]), "amd64")
            .expect("simulated machine")
            .to_stored();
        let mut duplicate = valid.clone();
        duplicate.gpus[1].id = duplicate.gpus[0].id.clone();
        let mut on_darwin = valid.clone();
        on_darwin.platform = Some(ExecutionPlatform::darwin_arm64().to_oci_platform());
        let mut no_platform = valid.clone();
        no_platform.platform = None;
        let mut no_memory = valid;
        no_memory.gpus[0].memory = None;

        let cases = [
            (
                duplicate,
                "machine description \"simulated-rtx-5090-rtx-4090\" declares GPU \
                 \"GPU-00000000-0000-0000-0000-000000005090\" more than once",
            ),
            (
                on_darwin,
                "machine description \"simulated-rtx-5090-rtx-4090\" has GPU \
                 \"GPU-00000000-0000-0000-0000-000000005090\", which needs a Linux platform",
            ),
            (
                no_platform,
                "stored machine description \"simulated-rtx-5090-rtx-4090\" has no valid platform",
            ),
            (
                no_memory,
                "stored machine description \"simulated-rtx-5090-rtx-4090\" has no valid GPU \
                 memory",
            ),
        ];
        for (stored, message) in cases {
            let error = MachineDescription::from_stored(&stored).expect_err(message);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn every_named_gpu_is_dedicated_with_a_simulated_host_driver() {
        for (name, _) in NAMED_GPUS {
            MachineDescription::simulated(&choices(&[name]), "amd64").expect(name);
        }
    }

    #[test]
    fn a_workload_uses_memory_on_its_own_dedicated_gpu_only() {
        const N: u64 = 3 * GIB + 7;
        let workload = MachineWorkload {
            gpu_bytes: BTreeMap::from([("GPU-00000000-0000-0000-0000-000000004080".to_owned(), N)]),
            host_bytes: 0,
        };

        let machine = MachineDescription::simulated(&choices(&["rtx-4090", "rtx-4080"]), "amd64")
            .expect("simulated machine");
        let idle = machine.render(&[], observed_at()).expect("snapshot");
        let busy = machine
            .render(&[workload], observed_at())
            .expect("snapshot");

        let used = |snapshot: &HardwareSnapshot| {
            dedicated_memory(snapshot)
                .into_iter()
                .map(|(_, _, _, used)| used)
                .collect::<Vec<_>>()
        };
        assert_eq!(used(&idle), vec![497 * MIB, 497 * MIB]);
        assert_eq!(used(&busy), vec![497 * MIB, 497 * MIB + N]);
        assert_eq!(host(&busy), host(&idle));
    }

    #[test]
    fn a_workload_on_a_unified_machine_uses_host_memory() {
        const N: u64 = 20 * GIB + 3;
        let on_gpu = MachineWorkload {
            gpu_bytes: BTreeMap::from([("GPU-00000000-0000-0000-0000-00000000gb10".to_owned(), N)]),
            host_bytes: 0,
        };
        let on_host = MachineWorkload {
            gpu_bytes: BTreeMap::new(),
            host_bytes: 5,
        };

        let idle = host(&render("gb10", &[])).available_memory_bytes;
        let busy = render("gb10", &[on_gpu, on_host]);

        assert_eq!(host(&busy).available_memory_bytes, idle - N - 5);
        assert_eq!(busy.accelerators, render("gb10", &[]).accelerators);
    }

    #[test]
    fn an_overcommitted_machine_shows_no_free_memory() {
        let workload = MachineWorkload {
            gpu_bytes: BTreeMap::new(),
            host_bytes: 200 * GIB,
        };

        assert_eq!(
            host(&render("mac-64g", &[workload])).available_memory_bytes,
            0
        );
    }

    #[test]
    fn a_workload_on_an_unknown_gpu_is_an_error() {
        let machine = MachineDescription::simulated(&choices(&["rtx-5090"]), "amd64")
            .expect("simulated machine");
        let workload = MachineWorkload {
            gpu_bytes: BTreeMap::from([("GPU-missing".to_owned(), 1)]),
            host_bytes: 0,
        };

        let error = machine
            .render(&[workload], observed_at())
            .expect_err("unknown GPU");

        assert_eq!(
            error.to_string(),
            "a workload uses GPU \"GPU-missing\", which machine \"simulated-rtx-5090\" doesn't have"
        );
    }

    #[test]
    fn a_machine_names_its_platform_and_gpus_and_checks_workloads() {
        let machine = MachineDescription::simulated(&choices(&["rtx-4090", "rtx-4080"]), "amd64")
            .expect("simulated machine");
        let on = |gpu_id: &str| MachineWorkload {
            gpu_bytes: BTreeMap::from([(gpu_id.to_owned(), 1)]),
            host_bytes: 0,
        };

        assert_eq!(machine.platform(), &ExecutionPlatform::linux_amd64());
        assert_eq!(
            machine.gpu_ids().collect::<Vec<_>>(),
            [
                "GPU-00000000-0000-0000-0000-000000004090",
                "GPU-00000000-0000-0000-0000-000000004080"
            ]
        );
        assert_eq!(
            machine.gpu_vendor("GPU-00000000-0000-0000-0000-000000004080"),
            Some(Accelerator::Nvidia)
        );
        assert_eq!(machine.gpu_vendor("GPU-missing"), None);
        machine
            .check_workload(&on("GPU-00000000-0000-0000-0000-000000004090"))
            .expect("workload on a GPU of the machine");
        assert_eq!(
            machine
                .check_workload(&on("GPU-missing"))
                .expect_err("unknown GPU")
                .to_string(),
            "a workload uses GPU \"GPU-missing\", which machine \"simulated-rtx-4090-rtx-4080\" \
             doesn't have"
        );
    }

    #[test]
    fn an_unknown_name_is_an_error_listing_the_known_machines() {
        let error = MachineDescription::named("rtx-9090").expect_err("unknown machine");

        assert_eq!(
            error.to_string(),
            "no named machine \"rtx-9090\"; known machines: gb10, mac-64g"
        );
    }

    #[test]
    fn an_inline_description_renders_like_a_named_one() {
        let machine = MachineDescription::from_yaml(
            "inline",
            r#"
platform: linux/amd64
cpuCount: 4
hostMemory: { totalBytes: 1000, idleUsedBytes: 100 }
gpus:
  - id: GPU-a
    vendor: nvidia
    model: Test GPU
    driverVersion: "580.173.02"
    memory: { architecture: dedicated, totalBytes: 500, reservedBytes: 10 }
"#,
        )
        .expect("inline machine");

        let snapshot = machine.render(&[], observed_at()).expect("snapshot");

        assert_eq!(host(&snapshot).host_name, "inline");
        assert_eq!(host(&snapshot).available_memory_bytes, 900);
        assert_eq!(dedicated_memory(&snapshot), vec![("GPU-a", 0, 500, 10)]);
    }

    #[test]
    fn invalid_descriptions_are_errors() {
        let gpu = "{ id: GPU-a, vendor: nvidia, model: M, memory: { architecture: unified } }";
        let duplicate = format!(
            "platform: linux/amd64\ncpuCount: 1\nhostMemory: {{ totalBytes: 1, idleUsedBytes: 0 }}\n\
             gpus: [{gpu}, {gpu}]"
        );
        let cases = [
            (
                "platform: linux/riscv\ncpuCount: 1\nhostMemory: { totalBytes: 1, idleUsedBytes: 0 }",
                "machine description \"bad\" is malformed",
            ),
            (
                "platform: linux/amd64\ncpuCount: 1\nhostMemory: { totalBytes: 1, idleUsedBytes: 0 }\n\
                 gpus: [{ id: a, vendor: amd, model: M, memory: { architecture: unified } }]",
                "machine description \"bad\" is malformed",
            ),
            (
                "platform: linux/amd64\ncpuCount: 1\nhostMemory: { totalBytes: 1, idleUsedBytes: 0 }\n\
                 extra: 1",
                "machine description \"bad\" is malformed",
            ),
            (
                duplicate.as_str(),
                "machine description \"bad\" declares GPU \"GPU-a\" more than once",
            ),
        ];

        for (yaml, message) in cases {
            let error = MachineDescription::from_yaml("bad", yaml).expect_err(yaml);
            assert_eq!(error.to_string(), message, "{yaml}");
        }
    }

    fn choices(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Each GPU's driver version, in snapshot order.
    fn drivers(snapshot: &HardwareSnapshot) -> Vec<String> {
        snapshot
            .accelerators
            .iter()
            .flat_map(|observation| match observation.result.as_ref() {
                Some(accelerator_observation::Result::Facts(set)) => set.facts.iter(),
                other => panic!("expected accelerator facts, got {other:?}"),
            })
            .map(|facts| facts.driver_or_runtime_version.clone())
            .collect()
    }

    fn render(name: &str, workloads: &[MachineWorkload]) -> HardwareSnapshot {
        MachineDescription::simulated(&choices(&[name]), "amd64")
            .expect("simulated machine")
            .render(workloads, observed_at())
            .expect("snapshot")
    }

    #[test]
    fn local_host_snapshot_becomes_a_shareable_description_that_round_trips() {
        let snapshot = HardwareSnapshot {
            observed_at: None,
            host: Some(HostObservation {
                result: Some(host_observation::Result::Facts(HostFacts {
                    host_name: "alice-workstation.example.ts.net".to_owned(),
                    operating_system_or_kernel: "NixOS 26.05 /home/alice".to_owned(),
                    cpu_count: 24,
                    total_memory_bytes: 64 * GIB,
                    available_memory_bytes: 60 * GIB,
                    cached_memory_bytes: Some(2 * GIB),
                    filesystems: vec![FilesystemFacts {
                        mountpoint: "/home/alice".to_owned(),
                        ..Default::default()
                    }],
                })),
            }),
            accelerators: vec![
                vendor_observation(
                    AcceleratorVendor::Nvidia,
                    vec![AcceleratorFacts {
                        vendor: AcceleratorVendor::Nvidia as i32,
                        model: "NVIDIA GeForce RTX 5090".to_owned(),
                        memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                        total_memory_bytes: Some(32_607 * MIB),
                        driver_or_runtime_version: "595.71.05".to_owned(),
                        device_id: "GPU-11111111-2222-3333-4444-555555555555".to_owned(),
                        vendor_index: Some(0),
                        used_memory_bytes: Some(497 * MIB),
                        working_set_limit_bytes: None,
                        supported_signals: Vec::new(),
                    }],
                ),
                AcceleratorObservation {
                    vendor: AcceleratorVendor::Apple as i32,
                    result: Some(accelerator_observation::Result::Unavailable(
                        Default::default(),
                    )),
                },
            ],
        };

        let machine =
            MachineDescription::from_snapshot(ExecutionPlatform::linux_amd64(), &snapshot)
                .expect("description");
        let yaml = machine.to_yaml().expect("yaml");

        assert_eq!(
            yaml,
            format!(
                "platform: linux/amd64
cpuCount: 24
hostMemory:
  totalBytes: {}
  idleUsedBytes: {}
gpus:
- id: GPU-11111111-2222-3333-4444-555555555555
  vendor: nvidia
  model: NVIDIA GeForce RTX 5090
  driverVersion: 595.71.05
  memory:
    architecture: dedicated
    totalBytes: {}
    reservedBytes: {}
",
                64 * GIB,
                4 * GIB,
                32_607 * MIB,
                497 * MIB
            )
        );
        assert_eq!(machine.name(), LOCAL_MACHINE_NAME);
        for private in ["alice", "example.ts.net", "/home", "NixOS"] {
            assert!(!yaml.contains(private), "{private:?} leaked into {yaml}");
        }
        assert_eq!(
            MachineDescription::from_yaml(machine.name(), &yaml).expect("reads back"),
            machine
        );
    }

    #[test]
    fn described_named_machines_write_the_yaml_they_load_from() {
        let mac = MachineDescription::named("mac-64g").expect("named machine");
        let gb10 = MachineDescription::named("gb10").expect("named machine");
        let rtx = MachineDescription::simulated(&choices(&["rtx-5090", "rtx-4090"]), "amd64")
            .expect("simulated machine");

        for machine in [mac, gb10, rtx] {
            let snapshot = machine.render(&[], observed_at()).expect("snapshot");
            let described =
                MachineDescription::from_snapshot(machine.platform().clone(), &snapshot)
                    .expect("description");

            assert_eq!(
                described.to_yaml().expect("yaml"),
                machine.to_yaml().expect("yaml"),
                "{}",
                machine.name()
            );
        }
    }

    #[test]
    fn describing_a_host_fails_without_host_facts_or_for_an_undescribable_gpu() {
        let mut snapshot = MachineDescription::named("gb10")
            .expect("named machine")
            .render(&[], observed_at())
            .expect("snapshot");
        snapshot.accelerators = vec![vendor_observation(
            AcceleratorVendor::Amd,
            vec![AcceleratorFacts {
                vendor: AcceleratorVendor::Amd as i32,
                device_id: "amd-0".to_owned(),
                ..Default::default()
            }],
        )];

        let error = MachineDescription::from_snapshot(ExecutionPlatform::linux_arm64(), &snapshot)
            .expect_err("AMD GPU");
        assert_eq!(
            error.to_string(),
            "the host's GPU \"amd-0\" can't be described: has a vendor Dotcore can't describe"
        );

        snapshot.host = None;
        let error = MachineDescription::from_snapshot(ExecutionPlatform::linux_arm64(), &snapshot)
            .expect_err("no host");
        assert!(matches!(error, MachineError::LocalHostUnavailable));
    }

    fn observed_at() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn host(snapshot: &HardwareSnapshot) -> HostFacts {
        match snapshot.host.as_ref().and_then(|host| host.result.as_ref()) {
            Some(host_observation::Result::Facts(facts)) => facts.clone(),
            other => panic!("expected host facts, got {other:?}"),
        }
    }

    /// Each dedicated GPU's ID, vendor index, total and used memory, in snapshot order.
    fn dedicated_memory(snapshot: &HardwareSnapshot) -> Vec<(&str, u32, u64, u64)> {
        snapshot
            .accelerators
            .iter()
            .flat_map(|observation| match observation.result.as_ref() {
                Some(accelerator_observation::Result::Facts(set)) => set.facts.iter(),
                other => panic!("expected accelerator facts, got {other:?}"),
            })
            .map(|facts| {
                assert_eq!(
                    facts.memory_architecture(),
                    AcceleratorMemoryArchitecture::Dedicated
                );
                (
                    facts.device_id.as_str(),
                    facts.vendor_index.expect("vendor index"),
                    facts.total_memory_bytes.expect("total"),
                    facts.used_memory_bytes.expect("used"),
                )
            })
            .collect()
    }

    fn vendor_observation(
        vendor: AcceleratorVendor,
        facts: Vec<AcceleratorFacts>,
    ) -> AcceleratorObservation {
        AcceleratorObservation {
            vendor: vendor as i32,
            result: Some(accelerator_observation::Result::Facts(
                AcceleratorFactsSet { facts },
            )),
        }
    }
}
