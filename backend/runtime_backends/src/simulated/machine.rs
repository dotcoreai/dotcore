//! The declared machine a node with simulated hardware runs on: the hardware it reports in place
//! of a real reading, with the memory its running simulated workloads take.
//!
//! Every simulated model workload of the node takes the memory its owner expects it to use, from
//! its start until its removal, and every running application unit takes a deterministic share of
//! host memory and CPU time drawn from the machine. The node's hardware observations, and the
//! per-process rows that memory attribution reads, then move as they would on the real machine.
//! Nothing here reads the host's hardware.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use dotcore_domain::{
    Accelerator, SiteNodeId,
    clock::Clock,
    hardware_observation::{
        AcceleratorProcessObservation, AcceleratorVendorProcessObservation,
        AcceleratorVendorProcessResult, HardwareSample, HardwareSnapshotSource,
        NodeHardwareObservation, ObservedAcceleratorProcess,
    },
    host_process::HostProcessIdentity,
};
use dotcore_node_hardware::{MachineDescription, MachineError, MachineWorkload};
use dotcore_proto::dotcore::observation::v1::{AcceleratorVendor, HardwareSnapshot};
use dotcore_runtime::{
    WorkloadExpectedMemory, WorkloadUnitResourceObservation, WorkloadUnitResources,
};

/// One running application unit takes this fraction of the machine's host memory: a 256th.
const APPLICATION_UNIT_HOST_MEMORY_DIVISOR: u64 = 256;
/// One running application unit keeps this fraction of one CPU busy: a quarter.
const APPLICATION_UNIT_CPU_DIVISOR: u128 = 4;

/// One node's declared machine and the simulated workloads running on it. Clones share the
/// running workloads, so the node's runtimes and its hardware collection see the same machine.
#[derive(Clone)]
pub struct DeclaredMachine {
    description: Arc<MachineDescription>,
    clock: Arc<dyn Clock>,
    workloads: Arc<Mutex<RunningWorkloads>>,
}

impl DeclaredMachine {
    /// Declares `description` as the node's machine, stamping its observations with `clock`.
    pub fn new(description: MachineDescription, clock: Arc<dyn Clock>) -> Self {
        Self {
            description: Arc::new(description),
            clock,
            workloads: Arc::default(),
        }
    }

    /// Returns the description of the machine.
    pub fn description(&self) -> &MachineDescription {
        &self.description
    }

    /// Starts one simulated workload process on `node`, which takes `memory` on the machine until
    /// it ends. Fails, starting nothing, when the memory names a GPU the machine doesn't have.
    pub(super) fn start_workload(
        &self,
        node: &SiteNodeId,
        memory: &WorkloadExpectedMemory,
    ) -> Result<HostProcessIdentity, MachineError> {
        let workload = MachineWorkload {
            gpu_bytes: memory.device_bytes.clone(),
            host_bytes: memory.host_bytes,
        };
        self.description.check_workload(&workload)?;

        let mut workloads = self.workloads.lock().expect("declared machine workloads");
        workloads.next_process += 1;
        // Process IDs above any Linux PID limit never name a real process of the host.
        let identity = HostProcessIdentity {
            managed_node_id: node.clone(),
            process_id: u32::MAX - workloads.next_process,
            process_start_token: u64::from(workloads.next_process),
        };
        workloads.running.insert(identity.clone(), workload);
        Ok(identity)
    }

    /// Ends a simulated workload process, releasing its memory on the machine.
    pub(super) fn end_workload(&self, identity: &HostProcessIdentity) {
        self.workloads
            .lock()
            .expect("declared machine workloads")
            .running
            .remove(identity);
    }

    /// Starts one simulated application unit process on `node`, which takes a fixed share of the
    /// machine's host memory until it ends and uses CPU time at a steady rate.
    pub(super) fn start_application_unit(&self, node: &SiteNodeId) -> ApplicationUnitProcess {
        let memory = WorkloadExpectedMemory {
            device_bytes: BTreeMap::new(),
            host_bytes: self.description.host_memory_bytes() / APPLICATION_UNIT_HOST_MEMORY_DIVISOR,
        };
        let identity = self
            .start_workload(node, &memory)
            .expect("an application unit uses no GPU, so every machine runs it");
        ApplicationUnitProcess {
            identity,
            started_at: self.clock.wall_now(),
            host_bytes: memory.host_bytes,
        }
    }

    /// Returns the machine's current wall time.
    pub(super) fn wall_now(&self) -> SystemTime {
        self.clock.wall_now()
    }

    /// Ends a simulated application unit process, releasing its memory on the machine.
    pub(super) fn end_application_unit(&self, process: &ApplicationUnitProcess) {
        self.end_workload(&process.identity);
    }

    /// The resources a running application unit process uses now: the host memory it takes and
    /// the CPU time it used since it started.
    pub(super) fn application_unit_resources(
        &self,
        process: &ApplicationUnitProcess,
    ) -> WorkloadUnitResourceObservation {
        let observed_at = self.clock.wall_now();
        let running = observed_at
            .duration_since(process.started_at)
            .unwrap_or_default();
        WorkloadUnitResourceObservation {
            observed_at,
            resources: WorkloadUnitResources::Available {
                cpu_time_ns: u64::try_from(running.as_nanos() / APPLICATION_UNIT_CPU_DIVISOR)
                    .unwrap_or(u64::MAX),
                memory_bytes: process.host_bytes,
            },
        }
    }

    /// One process row per GPU each running workload uses, grouped by the vendors of the
    /// machine's GPUs; a vendor whose GPUs run nothing reports no rows.
    fn accelerator_processes(&self, running: &RunningWorkloads) -> AcceleratorProcessObservation {
        let mut vendors: Vec<(Accelerator, Vec<ObservedAcceleratorProcess>)> = Vec::new();
        for gpu_id in self.description.gpu_ids() {
            let vendor = self
                .description
                .gpu_vendor(gpu_id)
                .expect("the machine lists its own GPU");
            let rows = running.workloads_on(gpu_id);
            match vendors.iter_mut().find(|(known, _)| *known == vendor) {
                Some((_, existing)) => existing.extend(rows),
                None => vendors.push((vendor, rows.collect())),
            }
        }
        AcceleratorProcessObservation::Observed(
            vendors
                .into_iter()
                .map(|(vendor, rows)| AcceleratorVendorProcessObservation {
                    vendor: match vendor {
                        Accelerator::Nvidia => AcceleratorVendor::Nvidia,
                        Accelerator::Apple => AcceleratorVendor::Apple,
                    },
                    result: AcceleratorVendorProcessResult::Observed(rows),
                })
                .collect(),
        )
    }

    fn render(&self, running: &RunningWorkloads) -> HardwareSnapshot {
        let workloads = running.running.values().cloned().collect::<Vec<_>>();
        self.description
            .render(&workloads, self.clock.wall_now())
            .expect("running workloads use only GPUs of the machine, checked when they start")
    }
}

impl HardwareSnapshotSource for DeclaredMachine {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        self.render(&self.workloads.lock().expect("declared machine workloads"))
    }

    fn sample(&self) -> HardwareSample {
        HardwareSample::from_snapshot(&self.collect_snapshot())
    }

    fn collect_observation(&self) -> NodeHardwareObservation {
        // One lock, so the snapshot and the process rows describe the same running workloads.
        let running = self.workloads.lock().expect("declared machine workloads");
        NodeHardwareObservation {
            snapshot: self.render(&running),
            accelerator_processes: self.accelerator_processes(&running),
        }
    }
}

impl fmt::Debug for DeclaredMachine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeclaredMachine")
            .field("machine", &self.description.name())
            .finish_non_exhaustive()
    }
}

/// One running simulated application unit process: its process on the machine, when it started
/// and the host memory it takes.
#[derive(Clone, Debug)]
pub(super) struct ApplicationUnitProcess {
    identity: HostProcessIdentity,
    started_at: SystemTime,
    host_bytes: u64,
}

/// The simulated workload processes running on a declared machine, and the memory each takes.
#[derive(Debug, Default)]
struct RunningWorkloads {
    next_process: u32,
    running: BTreeMap<HostProcessIdentity, MachineWorkload>,
}

impl RunningWorkloads {
    /// One process row per running workload that uses memory on `gpu_id`.
    fn workloads_on<'a>(
        &'a self,
        gpu_id: &'a str,
    ) -> impl Iterator<Item = ObservedAcceleratorProcess> + 'a {
        self.running.iter().filter_map(move |(identity, workload)| {
            workload
                .gpu_bytes
                .get(gpu_id)
                .map(|bytes| ObservedAcceleratorProcess {
                    device_id: gpu_id.to_owned(),
                    process_id: identity.process_id,
                    process_start_token: identity.process_start_token,
                    ancestors: Vec::new(),
                    used_memory_bytes: Some(*bytes),
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use dotcore_domain::clock::VirtualClock;
    use dotcore_proto::dotcore::observation::v1::host_observation;

    use super::*;

    const GPU_5090: &str = "GPU-00000000-0000-0000-0000-000000005090";
    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn collection_renders_the_machine_with_its_running_workloads() {
        let (machine, clock) = declared("rtx-5090");
        let node = SiteNodeId::for_test("node-a");
        let description = MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
            .expect("named machine");

        let first = machine
            .start_workload(&node, &on_gpu(GPU_5090, 3 * GIB, 1))
            .expect("first workload");
        let second = machine
            .start_workload(&node, &on_gpu(GPU_5090, 5 * GIB, 2))
            .expect("second workload");
        machine.end_workload(&first);

        let observation = machine.collect_observation();
        assert_eq!(
            observation.snapshot,
            description
                .render(&[workload(GPU_5090, 5 * GIB, 2)], clock.wall_now())
                .expect("expected snapshot")
        );
        assert_eq!(
            observation.accelerator_processes,
            AcceleratorProcessObservation::Observed(vec![AcceleratorVendorProcessObservation {
                vendor: AcceleratorVendor::Nvidia,
                result: AcceleratorVendorProcessResult::Observed(vec![
                    ObservedAcceleratorProcess {
                        device_id: GPU_5090.to_owned(),
                        process_id: second.process_id,
                        process_start_token: second.process_start_token,
                        ancestors: Vec::new(),
                        used_memory_bytes: Some(5 * GIB),
                    }
                ]),
            }])
        );
        assert_eq!(machine.collect_snapshot(), observation.snapshot);
        assert_eq!(
            machine.sample(),
            HardwareSample::from_snapshot(&observation.snapshot)
        );
        assert_eq!(second.managed_node_id, node);
        assert_ne!(first.process_id, second.process_id);
        assert!(second.process_id > 4_194_304, "above any Linux PID limit");
    }

    #[test]
    fn an_idle_machine_observes_no_process_on_each_vendor() {
        let (machine, clock) = declared("gb10");

        let observation = machine.collect_observation();

        assert_eq!(
            observation.snapshot,
            MachineDescription::named("gb10")
                .expect("named machine")
                .render(&[], clock.wall_now())
                .expect("idle snapshot")
        );
        assert_eq!(
            observation.accelerator_processes,
            AcceleratorProcessObservation::Observed(vec![AcceleratorVendorProcessObservation {
                vendor: AcceleratorVendor::Nvidia,
                result: AcceleratorVendorProcessResult::Observed(Vec::new()),
            }])
        );
    }

    #[test]
    fn a_unified_workload_lowers_host_memory_until_it_ends() {
        let (machine, _) = declared("mac-64g");
        let node = SiteNodeId::for_test("node-a");
        let idle = available_host_memory(&machine);

        let identity = machine
            .start_workload(&node, &on_gpu("apple-gpu-0", 20 * GIB, 3))
            .expect("unified workload");

        assert_eq!(available_host_memory(&machine), idle - 20 * GIB - 3);
        machine.end_workload(&identity);
        assert_eq!(available_host_memory(&machine), idle);
    }

    #[test]
    fn an_application_unit_takes_a_share_of_host_memory_and_steady_cpu_time() {
        let (machine, clock) = declared("rtx-5090");
        let node = SiteNodeId::for_test("node-a");
        let idle = available_host_memory(&machine);
        let unit_bytes = machine.description().host_memory_bytes() / 256;

        let process = machine.start_application_unit(&node);
        clock.advance(Duration::from_secs(8));
        let observation = machine.application_unit_resources(&process);

        assert_eq!(available_host_memory(&machine), idle - unit_bytes);
        assert_eq!(observation.observed_at, clock.wall_now());
        assert!(matches!(
            observation.resources,
            WorkloadUnitResources::Available {
                cpu_time_ns: 2_000_000_000,
                memory_bytes,
            } if memory_bytes == unit_bytes
        ));
        machine.end_application_unit(&process);
        assert_eq!(available_host_memory(&machine), idle);
    }

    #[test]
    fn a_workload_on_another_gpu_never_starts() {
        let (machine, _) = declared("rtx-5090");
        let idle = machine.collect_observation();

        let error = machine
            .start_workload(
                &SiteNodeId::for_test("node-a"),
                &on_gpu("GPU-missing", 1, 0),
            )
            .expect_err("unknown GPU");

        assert!(matches!(error, MachineError::UnknownGpu { .. }));
        assert_eq!(machine.collect_observation(), idle);
    }

    fn declared(name: &str) -> (DeclaredMachine, VirtualClock) {
        let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(1_800_000_000));
        (
            DeclaredMachine::new(
                MachineDescription::simulated(&[name.to_owned()], "amd64")
                    .expect("simulated machine"),
                Arc::new(clock.clone()),
            ),
            clock,
        )
    }

    fn on_gpu(gpu_id: &str, gpu_bytes: u64, host_bytes: u64) -> WorkloadExpectedMemory {
        WorkloadExpectedMemory {
            device_bytes: BTreeMap::from([(gpu_id.to_owned(), gpu_bytes)]),
            host_bytes,
        }
    }

    fn workload(gpu_id: &str, gpu_bytes: u64, host_bytes: u64) -> MachineWorkload {
        MachineWorkload {
            gpu_bytes: BTreeMap::from([(gpu_id.to_owned(), gpu_bytes)]),
            host_bytes,
        }
    }

    fn available_host_memory(machine: &DeclaredMachine) -> u64 {
        match machine.collect_snapshot().host.and_then(|host| host.result) {
            Some(host_observation::Result::Facts(facts)) => facts.available_memory_bytes,
            other => panic!("expected host facts, got {other:?}"),
        }
    }
}
