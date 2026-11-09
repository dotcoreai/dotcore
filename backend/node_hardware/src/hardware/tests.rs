use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use dotcore_domain::test_support::tracing::{Record, capture_records};
use dotcore_host_process::test_support::FakeProcessTable;
use dotcore_proto::dotcore::observation::v1::FilesystemFacts;
use tempfile::TempDir;

use super::*;

#[test]
fn system_host_facts_report_cpus_and_memory() {
    let facts = SystemHostFactsAdapter::new().collect_host_facts().unwrap();

    assert!(facts.cpu_count > 0);
    assert!(facts.total_memory_bytes > 0);
    assert!(facts.available_memory_bytes <= facts.total_memory_bytes);
}

struct FakeHost(Result<HostFacts, UnavailableReason>);

impl HostFactsAdapter for FakeHost {
    fn collect_host_facts(&self) -> Result<HostFacts, UnavailableReason> {
        self.0.clone()
    }
}

struct FakeAccelerator {
    vendor: AcceleratorVendor,
    result: Result<Vec<AcceleratorFacts>, UnavailableReason>,
}

impl AcceleratorVendorAdapter for FakeAccelerator {
    fn vendor(&self) -> AcceleratorVendor {
        self.vendor
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        self.result.clone().map_err(AcceleratorUnavailable::stable)
    }
}

impl AcceleratorVendorDiscoverer for FakeAccelerator {
    fn vendor(&self) -> AcceleratorVendor {
        self.vendor
    }

    fn discover(&self) -> AcceleratorDiscovery {
        match &self.result {
            Ok(facts) if facts.is_empty() => AcceleratorDiscovery::Absent,
            Ok(facts) => AcceleratorDiscovery::Detected(Box::new(StaticPoller {
                facts: facts.clone(),
            })),
            Err(reason) => AcceleratorDiscovery::Unavailable(*reason),
        }
    }
}

struct StaticPoller {
    facts: Vec<AcceleratorFacts>,
}

impl AcceleratorVendorPoller for StaticPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        Ok(AcceleratorVendorObservation {
            facts: self.facts.clone(),
            processes: Vec::new(),
        })
    }
}

struct CountingDiscoverer {
    vendor: AcceleratorVendor,
    discovery_calls: Arc<AtomicUsize>,
    poll_calls: Arc<AtomicUsize>,
    discovery: Mutex<Option<Result<Vec<AcceleratorFacts>, UnavailableReason>>>,
    panic: bool,
}

impl AcceleratorVendorDiscoverer for CountingDiscoverer {
    fn vendor(&self) -> AcceleratorVendor {
        self.vendor
    }

    fn discover(&self) -> AcceleratorDiscovery {
        self.discovery_calls.fetch_add(1, Ordering::SeqCst);
        if self.panic {
            panic!("injected discovery panic");
        }
        match self
            .discovery
            .lock()
            .expect("discovery outcome")
            .take()
            .expect("discoverer is called once")
        {
            Ok(facts) if facts.is_empty() => AcceleratorDiscovery::Absent,
            Ok(facts) => AcceleratorDiscovery::Detected(Box::new(CountingPoller {
                facts,
                calls: Arc::clone(&self.poll_calls),
            })),
            Err(reason) => AcceleratorDiscovery::Unavailable(reason),
        }
    }
}

struct PanickingPollDiscoverer {
    discovery_calls: Arc<AtomicUsize>,
    poll_calls: Arc<AtomicUsize>,
}

impl AcceleratorVendorDiscoverer for PanickingPollDiscoverer {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn discover(&self) -> AcceleratorDiscovery {
        self.discovery_calls.fetch_add(1, Ordering::SeqCst);
        AcceleratorDiscovery::Detected(Box::new(PanickingOncePoller {
            calls: Arc::clone(&self.poll_calls),
        }))
    }
}

struct PanickingOncePoller {
    calls: Arc<AtomicUsize>,
}

impl AcceleratorVendorPoller for PanickingOncePoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("injected poll panic");
        }
        Ok(AcceleratorVendorObservation {
            facts: nvidia_facts(),
            processes: Vec::new(),
        })
    }
}

struct CountingPoller {
    facts: Vec<AcceleratorFacts>,
    calls: Arc<AtomicUsize>,
}

impl AcceleratorVendorPoller for CountingPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AcceleratorVendorObservation {
            facts: self.facts.clone(),
            processes: Vec::new(),
        })
    }
}

struct RecoveringPollDiscoverer {
    discovery_calls: Arc<AtomicUsize>,
    poll_calls: Arc<AtomicUsize>,
    facts: Vec<AcceleratorFacts>,
}

impl AcceleratorVendorDiscoverer for RecoveringPollDiscoverer {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn discover(&self) -> AcceleratorDiscovery {
        self.discovery_calls.fetch_add(1, Ordering::SeqCst);
        AcceleratorDiscovery::Detected(Box::new(RecoveringPoller {
            calls: Arc::clone(&self.poll_calls),
            facts: self.facts.clone(),
        }))
    }
}

struct RecoveringPoller {
    calls: Arc<AtomicUsize>,
    facts: Vec<AcceleratorFacts>,
}

impl AcceleratorVendorPoller for RecoveringPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(UnavailableReason::CollectionFailed);
        }
        Ok(AcceleratorVendorObservation {
            facts: self.facts.clone(),
            processes: Vec::new(),
        })
    }
}

/// Discovers a device whose first poll signals its start and then waits for release.
struct BlockingPollDiscoverer {
    poll: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl AcceleratorVendorDiscoverer for BlockingPollDiscoverer {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn discover(&self) -> AcceleratorDiscovery {
        AcceleratorDiscovery::Detected(Box::new(BlockingPoller {
            poll: self.poll.lock().expect("poll gate").take(),
        }))
    }
}

struct BlockingPoller {
    poll: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

impl AcceleratorVendorPoller for BlockingPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        if let Some((entered, release)) = &self.poll {
            entered.send(()).expect("test waits for the poll");
            release.recv().expect("test releases the poll");
        }
        Ok(AcceleratorVendorObservation {
            facts: nvidia_facts(),
            processes: Vec::new(),
        })
    }

    fn sampler(&self) -> Option<Arc<dyn AcceleratorSampler>> {
        Some(Arc::new(FixedSampler))
    }
}

struct FixedSampler;

impl AcceleratorSampler for FixedSampler {
    fn sample(&self) -> Vec<AcceleratorSample> {
        vec![AcceleratorSample {
            memory_total_bytes: Some(1_000),
            memory_used_bytes: Some(400),
            ..AcceleratorSample::new("GPU-a".to_owned())
        }]
    }
}

/// Wall time fixed at 1_725_000_000 s and 123 ns.
#[derive(Debug)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    fn wall_now(&self) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::new(1_725_000_000, 123)
    }

    fn sleep_until(&self, deadline: tokio::time::Instant) -> dotcore_domain::clock::ClockSleep {
        Box::pin(tokio::time::sleep_until(deadline))
    }
}

#[derive(Clone)]
struct FakeInventory {
    device_count: u32,
    driver_version: String,
    models: Vec<String>,
    device_ids: Vec<String>,
    memories: Vec<Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable>>,
    processes: Vec<Vec<dotcore_nvml::ComputeProcessMemory>>,
    device_count_calls: Arc<AtomicUsize>,
    indexed_poll_calls: Arc<AtomicUsize>,
    uuid_poll_calls: Arc<AtomicUsize>,
}

impl FakeInventory {
    fn index_for_uuid(&self, device_uuid: &str) -> Result<usize, dotcore_nvml::Unavailable> {
        self.device_ids
            .iter()
            .position(|candidate| candidate == device_uuid)
            .ok_or_else(|| {
                dotcore_nvml::Unavailable::from_kind(dotcore_nvml::UnavailableKind::Initialization)
            })
    }
}

impl NvidiaObservationSource for FakeInventory {
    fn device_count(&self) -> Result<u32, dotcore_nvml::Unavailable> {
        self.device_count_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.device_count)
    }

    fn driver_version(&self) -> Result<String, dotcore_nvml::Unavailable> {
        Ok(self.driver_version.clone())
    }

    fn device_model(&self, index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        self.indexed_poll_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.models[index as usize].clone())
    }

    fn device_uuid(&self, index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        Ok(self.device_ids[index as usize].clone())
    }

    fn device_model_by_uuid(&self, device_uuid: &str) -> Result<String, dotcore_nvml::Unavailable> {
        self.uuid_poll_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.models[self.index_for_uuid(device_uuid)?].clone())
    }

    fn device_memory(
        &self,
        index: u32,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        self.indexed_poll_calls.fetch_add(1, Ordering::SeqCst);
        self.memories[index as usize].clone()
    }

    fn device_memory_by_uuid(
        &self,
        device_uuid: &str,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        self.uuid_poll_calls.fetch_add(1, Ordering::SeqCst);
        self.memories[self.index_for_uuid(device_uuid)?].clone()
    }

    fn device_compute_processes(
        &self,
        index: u32,
    ) -> Result<Vec<dotcore_nvml::ComputeProcessMemory>, dotcore_nvml::Unavailable> {
        self.indexed_poll_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.processes[index as usize].clone())
    }

    fn device_compute_processes_by_uuid(
        &self,
        device_uuid: &str,
    ) -> Result<Vec<dotcore_nvml::ComputeProcessMemory>, dotcore_nvml::Unavailable> {
        self.uuid_poll_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.processes[self.index_for_uuid(device_uuid)?].clone())
    }
}

#[derive(Clone)]
struct ReorderedIndexInventory {
    index_order: Arc<Mutex<Vec<String>>>,
    indexed_poll_calls: Arc<AtomicUsize>,
}

impl ReorderedIndexInventory {
    fn model(device_uuid: &str) -> Result<String, dotcore_nvml::Unavailable> {
        match device_uuid {
            "GPU-a" => Ok("Fixture GPU A".to_owned()),
            "GPU-b" => Ok("Fixture GPU B".to_owned()),
            _ => Err(dotcore_nvml::Unavailable::from_kind(
                dotcore_nvml::UnavailableKind::Initialization,
            )),
        }
    }

    fn memory(device_uuid: &str) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        match device_uuid {
            "GPU-a" => Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 1_000,
                used_bytes: 100,
            }),
            "GPU-b" => Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 2_000,
                used_bytes: 200,
            }),
            _ => Err(dotcore_nvml::Unavailable::from_kind(
                dotcore_nvml::UnavailableKind::Initialization,
            )),
        }
    }

    fn indexed_uuid(&self, index: u32) -> String {
        self.index_order.lock().expect("index order")[index as usize].clone()
    }
}

impl NvidiaObservationSource for ReorderedIndexInventory {
    fn device_count(&self) -> Result<u32, dotcore_nvml::Unavailable> {
        Ok(2)
    }

    fn driver_version(&self) -> Result<String, dotcore_nvml::Unavailable> {
        Ok("570.42.01".to_owned())
    }

    fn device_model(&self, index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        self.indexed_poll_calls.fetch_add(1, Ordering::SeqCst);
        Self::model(&self.indexed_uuid(index))
    }

    fn device_uuid(&self, index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        Ok(self.indexed_uuid(index))
    }

    fn device_model_by_uuid(&self, device_uuid: &str) -> Result<String, dotcore_nvml::Unavailable> {
        Self::model(device_uuid)
    }

    fn device_memory(
        &self,
        index: u32,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        self.indexed_poll_calls.fetch_add(1, Ordering::SeqCst);
        Self::memory(&self.indexed_uuid(index))
    }

    fn device_memory_by_uuid(
        &self,
        device_uuid: &str,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        Self::memory(device_uuid)
    }

    fn device_compute_processes(
        &self,
        _index: u32,
    ) -> Result<Vec<dotcore_nvml::ComputeProcessMemory>, dotcore_nvml::Unavailable> {
        self.indexed_poll_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    fn device_compute_processes_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<Vec<dotcore_nvml::ComputeProcessMemory>, dotcore_nvml::Unavailable> {
        Ok(Vec::new())
    }
}

struct MissingFixedUuidInventory {
    device_count_calls: Arc<AtomicUsize>,
    uuid_poll_calls: Arc<AtomicUsize>,
}

impl NvidiaObservationSource for MissingFixedUuidInventory {
    fn device_count(&self) -> Result<u32, dotcore_nvml::Unavailable> {
        self.device_count_calls.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }

    fn driver_version(&self) -> Result<String, dotcore_nvml::Unavailable> {
        Ok("570.42.01".to_owned())
    }

    fn device_model(&self, _index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        panic!("fixed polling must not query model by index")
    }

    fn device_uuid(&self, index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        assert_eq!(index, 0);
        Ok("GPU-stable".to_owned())
    }

    fn device_model_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<String, dotcore_nvml::Unavailable> {
        self.uuid_poll_calls.fetch_add(1, Ordering::SeqCst);
        Err(dotcore_nvml::Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Initialization,
        ))
    }

    fn device_memory(
        &self,
        _index: u32,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        panic!("fixed polling must not query memory by index")
    }

    fn device_memory_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        self.uuid_poll_calls.fetch_add(1, Ordering::SeqCst);
        Err(dotcore_nvml::Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Initialization,
        ))
    }

    fn device_compute_processes(
        &self,
        _index: u32,
    ) -> Result<Vec<dotcore_nvml::ComputeProcessMemory>, dotcore_nvml::Unavailable> {
        panic!("fixed polling must not query processes by index")
    }
}

/// NVIDIA devices whose per-second readings come from one queue per device, the last
/// reading repeating; discovery's support probe takes the first.
struct SignalInventory {
    device_ids: Vec<String>,
    samples: Mutex<BTreeMap<String, Vec<DeviceSampleResult>>>,
}

type DeviceSampleResult = Result<dotcore_nvml::DeviceSample, dotcore_nvml::Unavailable>;

impl NvidiaObservationSource for SignalInventory {
    fn device_count(&self) -> Result<u32, dotcore_nvml::Unavailable> {
        Ok(self.device_ids.len() as u32)
    }

    fn driver_version(&self) -> Result<String, dotcore_nvml::Unavailable> {
        Ok("580.173.02".to_owned())
    }

    fn device_model(&self, _index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        Ok("Fixture GPU".to_owned())
    }

    fn device_uuid(&self, index: u32) -> Result<String, dotcore_nvml::Unavailable> {
        Ok(self.device_ids[index as usize].clone())
    }

    fn device_model_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<String, dotcore_nvml::Unavailable> {
        Ok("Fixture GPU".to_owned())
    }

    fn device_memory(
        &self,
        _index: u32,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        Ok(dotcore_nvml::DeviceMemory::Unified)
    }

    fn device_memory_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable> {
        Ok(dotcore_nvml::DeviceMemory::Unified)
    }

    fn device_sample_by_uuid(&self, device_uuid: &str) -> DeviceSampleResult {
        let mut samples = self.samples.lock().expect("device samples");
        let queue = samples.get_mut(device_uuid).expect("fixture device");
        if queue.len() > 1 {
            queue.remove(0)
        } else {
            queue[0].clone()
        }
    }

    fn device_compute_processes_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<Vec<dotcore_nvml::ComputeProcessMemory>, dotcore_nvml::Unavailable> {
        Ok(Vec::new())
    }
}

fn signal_adapter<const N: usize>(
    devices: [(&str, Vec<DeviceSampleResult>); N],
) -> SystemNvidiaAcceleratorAdapter {
    let inventory = Mutex::new(Some(SignalInventory {
        device_ids: devices.iter().map(|(id, _)| (*id).to_owned()).collect(),
        samples: Mutex::new(
            devices
                .into_iter()
                .map(|(id, samples)| (id.to_owned(), samples))
                .collect(),
        ),
    }));
    SystemNvidiaAcceleratorAdapter {
        open_inventory: Box::new(move || {
            Ok(Box::new(
                inventory
                    .lock()
                    .expect("inventory")
                    .take()
                    .expect("discovery opens the inventory once"),
            ))
        }),
        processes: Arc::new(HostProcessTable),
    }
}

fn device_sample(
    memory: Result<dotcore_nvml::DeviceMemory, dotcore_nvml::Unavailable>,
    utilization_percent: Result<u32, dotcore_nvml::Unavailable>,
    temperature_celsius: Result<u32, dotcore_nvml::Unavailable>,
    power_milliwatts: Result<u32, dotcore_nvml::Unavailable>,
) -> dotcore_nvml::DeviceSample {
    dotcore_nvml::DeviceSample {
        memory,
        utilization_percent,
        temperature_celsius,
        power_milliwatts,
    }
}

fn nvml_error(kind: dotcore_nvml::UnavailableKind) -> dotcore_nvml::Unavailable {
    dotcore_nvml::Unavailable::from_kind(kind)
}

fn nvidia_adapter_for(inventory: FakeInventory) -> SystemNvidiaAcceleratorAdapter {
    nvidia_adapter_with_processes(inventory, Arc::new(HostProcessTable))
}

fn nvidia_adapter_with_processes(
    inventory: FakeInventory,
    processes: Arc<dyn ProcessTable>,
) -> SystemNvidiaAcceleratorAdapter {
    SystemNvidiaAcceleratorAdapter {
        open_inventory: Box::new(move || Ok(Box::new(inventory.clone()))),
        processes,
    }
}

#[test]
fn nvidia_adapter_reports_one_fact_per_device_with_per_device_values() {
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 2,
        driver_version: "570.42.01".to_owned(),
        models: vec![
            "NVIDIA GeForce RTX 5090".to_owned(),
            "NVIDIA RTX PRO 6000 Blackwell".to_owned(),
        ],
        device_ids: vec!["GPU-stable-a".to_owned(), "GPU-stable-b".to_owned()],
        memories: vec![
            Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 32_000_000_000,
                used_bytes: 12_000_000_000,
            }),
            Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 96_000_000_000,
                used_bytes: 48_000_000_000,
            }),
        ],
        processes: vec![Vec::new(), Vec::new()],
        device_count_calls: Arc::new(AtomicUsize::new(0)),
        indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
        uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
    });

    let facts = adapter.collect_accelerator_facts().expect("NVIDIA facts");

    assert_eq!(facts.len(), 2);
    assert!(
        facts
            .iter()
            .all(|fact| fact.vendor == AcceleratorVendor::Nvidia as i32)
    );
    assert_eq!(
        &facts[0],
        &AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "NVIDIA GeForce RTX 5090".to_owned(),
            memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
            total_memory_bytes: Some(32_000_000_000),
            driver_or_runtime_version: "570.42.01".to_owned(),
            device_id: "GPU-stable-a".to_owned(),
            vendor_index: Some(0),
            used_memory_bytes: Some(12_000_000_000),
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        }
    );
    assert_eq!(
        &facts[1],
        &AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "NVIDIA RTX PRO 6000 Blackwell".to_owned(),
            memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
            total_memory_bytes: Some(96_000_000_000),
            driver_or_runtime_version: "570.42.01".to_owned(),
            device_id: "GPU-stable-b".to_owned(),
            vendor_index: Some(1),
            used_memory_bytes: Some(48_000_000_000),
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        }
    );
}

#[test]
fn nvidia_adapter_reports_canonical_device_and_lifetime_safe_process_memory() {
    let processes = FakeProcessTable::new([(1, 0, 1), (41, 1, 700), (1275, 41, 800)]);
    let adapter = nvidia_adapter_with_processes(
        FakeInventory {
            device_count: 1,
            driver_version: "570.42.01".to_owned(),
            models: vec!["Fixture GPU".to_owned()],
            device_ids: vec!["GPU-stable".to_owned()],
            memories: vec![Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 1_000,
                used_bytes: 400,
            })],
            processes: vec![vec![
                dotcore_nvml::ComputeProcessMemory {
                    process_id: 1275,
                    used_memory_bytes: Some(123),
                },
                dotcore_nvml::ComputeProcessMemory {
                    process_id: 4242,
                    used_memory_bytes: Some(77),
                },
            ]],
            device_count_calls: Arc::new(AtomicUsize::new(0)),
            indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
            uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
        },
        Arc::new(processes),
    );

    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPU must be detected");
    };
    let observation = poller.poll().expect("NVIDIA observation");

    assert_eq!(observation.facts[0].device_id, "GPU-stable");
    assert_eq!(
        observation.processes,
        vec![ObservedAcceleratorProcess {
            device_id: "GPU-stable".to_owned(),
            process_id: 1275,
            process_start_token: 800,
            ancestors: vec![
                ProcessLifetime {
                    process_id: 41,
                    start_token: 700,
                },
                ProcessLifetime {
                    process_id: 1,
                    start_token: 1,
                },
            ],
            used_memory_bytes: Some(123),
        }]
    );
}

#[test]
fn nvidia_discovery_enumerates_once_and_every_poll_uses_fixed_uuid() {
    let device_count_calls = Arc::new(AtomicUsize::new(0));
    let indexed_poll_calls = Arc::new(AtomicUsize::new(0));
    let uuid_poll_calls = Arc::new(AtomicUsize::new(0));
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 1,
        driver_version: "570.42.01".to_owned(),
        models: vec!["Fixture GPU".to_owned()],
        device_ids: vec!["GPU-stable".to_owned()],
        memories: vec![Ok(dotcore_nvml::DeviceMemory::Dedicated {
            total_bytes: 1_000,
            used_bytes: 400,
        })],
        processes: vec![Vec::new()],
        device_count_calls: Arc::clone(&device_count_calls),
        indexed_poll_calls: Arc::clone(&indexed_poll_calls),
        uuid_poll_calls: Arc::clone(&uuid_poll_calls),
    });
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPU must be detected");
    };

    let first = poller.poll().expect("first UUID poll");
    let second = poller.poll().expect("second UUID poll");

    assert_eq!(first.facts[0].device_id, "GPU-stable");
    assert_eq!(second.facts[0].device_id, "GPU-stable");
    assert_eq!(device_count_calls.load(Ordering::SeqCst), 1);
    assert_eq!(indexed_poll_calls.load(Ordering::SeqCst), 0);
    // One signal probe at discovery, then model, memory and two process reads per poll.
    assert_eq!(uuid_poll_calls.load(Ordering::SeqCst), 9);
}

#[test]
fn nvidia_index_reorder_cannot_substitute_a_fixed_uuid() {
    let index_order = Arc::new(Mutex::new(vec!["GPU-a".to_owned(), "GPU-b".to_owned()]));
    let indexed_poll_calls = Arc::new(AtomicUsize::new(0));
    let inventory = ReorderedIndexInventory {
        index_order: Arc::clone(&index_order),
        indexed_poll_calls: Arc::clone(&indexed_poll_calls),
    };
    let adapter = SystemNvidiaAcceleratorAdapter {
        open_inventory: Box::new(move || Ok(Box::new(inventory.clone()))),
        processes: Arc::new(HostProcessTable),
    };
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPUs must be detected");
    };
    *index_order.lock().expect("index order") = vec!["GPU-b".to_owned(), "GPU-a".to_owned()];

    let observation = poller.poll().expect("UUID poll after index reorder");

    assert_eq!(
        observation.facts,
        vec![
            AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: "Fixture GPU A".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                total_memory_bytes: Some(1_000),
                driver_or_runtime_version: "570.42.01".to_owned(),
                device_id: "GPU-a".to_owned(),
                vendor_index: Some(0),
                used_memory_bytes: Some(100),
                working_set_limit_bytes: None,
                supported_signals: Vec::new(),
            },
            AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: "Fixture GPU B".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                total_memory_bytes: Some(2_000),
                driver_or_runtime_version: "570.42.01".to_owned(),
                device_id: "GPU-b".to_owned(),
                vendor_index: Some(1),
                used_memory_bytes: Some(200),
                working_set_limit_bytes: None,
                supported_signals: Vec::new(),
            },
        ]
    );
    assert_eq!(indexed_poll_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn missing_fixed_nvidia_uuid_never_shrinks_or_rediscoveries_the_set() {
    let device_count_calls = Arc::new(AtomicUsize::new(0));
    let uuid_poll_calls = Arc::new(AtomicUsize::new(0));
    let inventory = MissingFixedUuidInventory {
        device_count_calls: Arc::clone(&device_count_calls),
        uuid_poll_calls: Arc::clone(&uuid_poll_calls),
    };
    let adapter = SystemNvidiaAcceleratorAdapter {
        open_inventory: Box::new(move || {
            Ok(Box::new(MissingFixedUuidInventory {
                device_count_calls: Arc::clone(&inventory.device_count_calls),
                uuid_poll_calls: Arc::clone(&inventory.uuid_poll_calls),
            }))
        }),
        processes: Arc::new(HostProcessTable),
    };
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPU must be detected");
    };

    assert!(matches!(
        poller.poll(),
        Err(UnavailableReason::CollectionFailed)
    ));
    assert!(matches!(
        poller.poll(),
        Err(UnavailableReason::CollectionFailed)
    ));
    assert_eq!(device_count_calls.load(Ordering::SeqCst), 1);
    // One signal probe at discovery, then one failing read per poll.
    assert_eq!(uuid_poll_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn nvidia_adapter_accepts_unified_memory_without_a_dedicated_total() {
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 1,
        driver_version: "580.173.02".to_owned(),
        models: vec!["NVIDIA GB10".to_owned()],
        device_ids: vec!["GPU-gb10".to_owned()],
        memories: vec![Ok(dotcore_nvml::DeviceMemory::Unified)],
        processes: vec![Vec::new()],
        device_count_calls: Arc::new(AtomicUsize::new(0)),
        indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
        uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
    });

    assert_eq!(
        adapter
            .collect_accelerator_facts()
            .expect("unified-memory NVIDIA facts"),
        vec![AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "NVIDIA GB10".to_owned(),
            memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
            total_memory_bytes: None,
            driver_or_runtime_version: "580.173.02".to_owned(),
            device_id: "GPU-gb10".to_owned(),
            vendor_index: Some(0),
            used_memory_bytes: None,
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        }]
    );
}

#[test]
fn nvidia_poller_reports_unified_memory_without_device_totals() {
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 1,
        driver_version: "580.173.02".to_owned(),
        models: vec!["NVIDIA GB10".to_owned()],
        device_ids: vec!["GPU-gb10".to_owned()],
        memories: vec![Ok(dotcore_nvml::DeviceMemory::Unified)],
        processes: vec![Vec::new()],
        device_count_calls: Arc::new(AtomicUsize::new(0)),
        indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
        uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
    });
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("GB10 fixture must be detected");
    };

    assert_eq!(
        poller.poll().expect("unified-memory NVIDIA poll").facts,
        vec![AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "NVIDIA GB10".to_owned(),
            memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
            total_memory_bytes: None,
            driver_or_runtime_version: "580.173.02".to_owned(),
            device_id: "GPU-gb10".to_owned(),
            vendor_index: Some(0),
            used_memory_bytes: None,
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        }]
    );
}

#[test]
fn nvidia_adapter_keeps_other_memory_failures_terminal() {
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 1,
        driver_version: "580.173.02".to_owned(),
        models: vec!["NVIDIA GB10".to_owned()],
        device_ids: vec!["GPU-gb10".to_owned()],
        memories: vec![Err(dotcore_nvml::Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::PermissionDenied,
        ))],
        processes: vec![Vec::new()],
        device_count_calls: Arc::new(AtomicUsize::new(0)),
        indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
        uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
    });

    let error = adapter
        .collect_accelerator_facts()
        .expect_err("memory permission failure remains terminal");
    assert_eq!(error.reason(), UnavailableReason::PermissionDenied);
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn nvidia_adapter_reports_no_device_when_there_are_no_devices() {
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 0,
        driver_version: "570.42.01".to_owned(),
        models: vec![],
        device_ids: vec![],
        memories: vec![],
        processes: vec![],
        device_count_calls: Arc::new(AtomicUsize::new(0)),
        indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
        uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
    });

    assert_eq!(
        adapter
            .collect_accelerator_facts()
            .expect_err("an empty NVIDIA inventory should be unavailable")
            .reason(),
        UnavailableReason::NoDevice
    );
}

#[test]
fn nvidia_adapter_maps_open_failures_to_unavailable_reasons() {
    let cases = [
        (
            dotcore_nvml::UnavailableKind::MissingLibrary,
            UnavailableReason::MissingLibrary,
        ),
        (
            dotcore_nvml::UnavailableKind::PermissionDenied,
            UnavailableReason::PermissionDenied,
        ),
        (
            dotcore_nvml::UnavailableKind::Unsupported,
            UnavailableReason::Unsupported,
        ),
        (
            dotcore_nvml::UnavailableKind::Initialization,
            UnavailableReason::CollectionFailed,
        ),
    ];

    for (nvml_kind, expected) in cases {
        let nvml_unavailable = dotcore_nvml::Unavailable::from_kind(nvml_kind);
        let adapter = SystemNvidiaAcceleratorAdapter {
            open_inventory: Box::new(move || Err(nvml_unavailable.clone().into())),
            processes: Arc::new(HostProcessTable),
        };

        let error = adapter
            .collect_accelerator_facts()
            .expect_err("NVML open failure should be unavailable");
        assert_eq!(
            error.reason(),
            expected,
            "{nvml_kind:?} must map to {expected:?}"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "NVML source must be retained"
        );
    }
}

fn amd_fixture() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
        .expect("temp sysfs fixture");
    let root = dir.path().to_path_buf();
    let card0 = root.join("sys/class/drm/card0/device");
    let card1 = root.join("sys/class/drm/card1/device");
    fs::create_dir_all(&card0).expect("create card0");
    fs::create_dir_all(&card1).expect("create card1");
    fs::create_dir_all(root.join("sys/module/amdgpu")).expect("create amdgpu module directory");
    fs::write(card0.join("vendor"), "0x1002\n").expect("write vendor");
    fs::write(card0.join("device"), "0x744c\n").expect("write device");
    fs::write(card0.join("mem_info_vram_total"), "32233627136\n").expect("write vram total");
    fs::write(card1.join("vendor"), "0x1002\n").expect("write vendor");
    fs::write(card1.join("device"), "0x68b9\n").expect("write device");
    fs::write(root.join("sys/module/amdgpu/version"), "6.9.0\n").expect("write module version");
    (dir, root)
}

#[test]
fn amd_adapter_reports_one_fact_per_amd_card_with_module_version() {
    let (_dir, root) = amd_fixture();
    let adapter = SystemAmdAcceleratorAdapter::with_root(&root);

    let facts = adapter.collect_accelerator_facts().expect("AMD facts");

    assert_eq!(
        facts,
        vec![
            AcceleratorFacts {
                vendor: AcceleratorVendor::Amd as i32,
                model: "1002:68b9".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Unspecified as i32,
                total_memory_bytes: None,
                driver_or_runtime_version: "6.9.0".to_owned(),
                device_id: String::new(),
                vendor_index: None,
                used_memory_bytes: None,
                working_set_limit_bytes: None,
                supported_signals: Vec::new(),
            },
            AcceleratorFacts {
                vendor: AcceleratorVendor::Amd as i32,
                model: "1002:744c".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                total_memory_bytes: Some(32_233_627_136),
                driver_or_runtime_version: "6.9.0".to_owned(),
                device_id: String::new(),
                vendor_index: None,
                used_memory_bytes: None,
                working_set_limit_bytes: None,
                supported_signals: Vec::new(),
            },
        ]
    );
}

#[test]
fn amd_sampling_reads_dedicated_memory_of_cards_with_a_unique_id() {
    let (_dir, root) = amd_fixture();
    let card0 = root.join("sys/class/drm/card0/device");
    let card2 = root.join("sys/class/drm/card2/device");
    fs::create_dir_all(&card2).expect("create card2");
    fs::write(card0.join("unique_id"), "a1b2c3d4e5f60708\n").expect("write unique id");
    fs::write(card0.join("mem_info_vram_used"), "4294967296\n").expect("write vram used");
    fs::write(card2.join("vendor"), "0x1002\n").expect("write vendor");
    fs::write(card2.join("device"), "0x73bf\n").expect("write device");
    fs::write(card2.join("unique_id"), "0011223344556677\n").expect("write unique id");
    let adapter = SystemAmdAcceleratorAdapter::with_root(&root);

    // Card 1 has no unique id, so it has no identity to sample under.
    assert_eq!(
        adapter.sample().expect("AMD sample"),
        vec![
            AcceleratorSample {
                memory_total_bytes: None,
                memory_used_bytes: None,
                ..AcceleratorSample::new("0011223344556677".to_owned())
            },
            AcceleratorSample {
                memory_total_bytes: Some(32_233_627_136),
                memory_used_bytes: Some(4_294_967_296),
                ..AcceleratorSample::new("a1b2c3d4e5f60708".to_owned())
            },
        ]
    );
    let facts = adapter.collect_accelerator_facts().expect("AMD facts");
    assert_eq!(
        facts
            .iter()
            .map(|fact| (fact.model.as_str(), fact.device_id.as_str()))
            .collect::<Vec<_>>(),
        [
            ("1002:68b9", ""),
            ("1002:73bf", "0011223344556677"),
            ("1002:744c", "a1b2c3d4e5f60708"),
        ]
    );
}

#[test]
fn nvidia_sampling_reads_only_the_memory_of_the_discovered_devices() {
    let uuid_poll_calls = Arc::new(AtomicUsize::new(0));
    let indexed_poll_calls = Arc::new(AtomicUsize::new(0));
    let adapter = nvidia_adapter_for(FakeInventory {
        device_count: 2,
        driver_version: "570.42.01".to_owned(),
        models: vec!["Fixture GPU".to_owned(), "NVIDIA GB10".to_owned()],
        device_ids: vec!["GPU-a".to_owned(), "GPU-gb10".to_owned()],
        memories: vec![
            Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 1_000,
                used_bytes: 400,
            }),
            Ok(dotcore_nvml::DeviceMemory::Unified),
        ],
        processes: vec![Vec::new(), Vec::new()],
        device_count_calls: Arc::new(AtomicUsize::new(0)),
        indexed_poll_calls: Arc::clone(&indexed_poll_calls),
        uuid_poll_calls: Arc::clone(&uuid_poll_calls),
    });
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPUs must be detected");
    };
    uuid_poll_calls.store(0, Ordering::SeqCst);

    let sample = poller.sampler().expect("NVIDIA sampler").sample();

    assert_eq!(
        sample,
        vec![
            AcceleratorSample {
                memory_total_bytes: Some(1_000),
                memory_used_bytes: Some(400),
                ..AcceleratorSample::new("GPU-a".to_owned())
            },
            AcceleratorSample {
                memory_total_bytes: None,
                memory_used_bytes: None,
                ..AcceleratorSample::new("GPU-gb10".to_owned())
            },
        ]
    );
    // One device read per device: no model, driver, or process queries.
    assert_eq!(uuid_poll_calls.load(Ordering::SeqCst), 2);
    assert_eq!(indexed_poll_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failing_or_unsupported_signal_leaves_other_signals_and_devices_sampled() {
    let ok_device = || {
        Ok(device_sample(
            Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 1_000,
                used_bytes: 400,
            }),
            Ok(50),
            Ok(61),
            Ok(250_000),
        ))
    };
    let adapter = signal_adapter([
        (
            "GPU-a",
            vec![
                ok_device(),
                Ok(device_sample(
                    Ok(dotcore_nvml::DeviceMemory::Dedicated {
                        total_bytes: 1_000,
                        used_bytes: 400,
                    }),
                    Ok(50),
                    Err(nvml_error(dotcore_nvml::UnavailableKind::Initialization)),
                    Ok(250_000),
                )),
            ],
        ),
        (
            "GPU-b",
            vec![
                ok_device(),
                Err(nvml_error(dotcore_nvml::UnavailableKind::Initialization)),
            ],
        ),
        // GB10: unified memory and power queries the device does not support.
        (
            "GPU-gb10",
            vec![Ok(device_sample(
                Ok(dotcore_nvml::DeviceMemory::Unified),
                Ok(10),
                Ok(45),
                Err(nvml_error(dotcore_nvml::UnavailableKind::Unsupported)),
            ))],
        ),
    ]);
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPUs must be detected");
    };

    assert_eq!(
        poller.sampler().expect("NVIDIA sampler").sample(),
        vec![
            AcceleratorSample {
                memory_total_bytes: Some(1_000),
                memory_used_bytes: Some(400),
                utilization: Some(0.5),
                temperature_celsius: None,
                power_watts: Some(250.0),
                ..AcceleratorSample::new("GPU-a".to_owned())
            },
            AcceleratorSample {
                utilization: Some(0.1),
                temperature_celsius: Some(45.0),
                ..AcceleratorSample::new("GPU-gb10".to_owned())
            },
        ]
    );
}

#[test]
fn only_a_signal_the_device_does_not_support_is_left_out_of_its_supported_signals() {
    let adapter = signal_adapter([
        (
            "GPU-a",
            vec![Ok(device_sample(
                Ok(dotcore_nvml::DeviceMemory::Unified),
                Ok(10),
                Err(nvml_error(dotcore_nvml::UnavailableKind::Initialization)),
                Err(nvml_error(dotcore_nvml::UnavailableKind::Unsupported)),
            ))],
        ),
        // A failed lookup while probing says nothing about support.
        (
            "GPU-b",
            vec![Err(nvml_error(
                dotcore_nvml::UnavailableKind::Initialization,
            ))],
        ),
    ]);
    let AcceleratorDiscovery::Detected(poller) = adapter.discover() else {
        panic!("fixture GPUs must be detected");
    };

    let observation = poller.poll().expect("NVIDIA poll");

    assert_eq!(
        observation
            .facts
            .iter()
            .map(|facts| (facts.device_id.as_str(), facts.supported_signals.clone()))
            .collect::<Vec<_>>(),
        [
            (
                "GPU-a",
                vec![
                    AcceleratorSignal::Utilization as i32,
                    AcceleratorSignal::Temperature as i32,
                ]
            ),
            (
                "GPU-b",
                vec![
                    AcceleratorSignal::Utilization as i32,
                    AcceleratorSignal::Temperature as i32,
                    AcceleratorSignal::Power as i32,
                ]
            ),
        ]
    );
}

#[test]
fn a_collector_sample_reads_host_memory_and_only_discovered_accelerators() {
    let collector = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![Box::new(nvidia_adapter_for(FakeInventory {
            device_count: 1,
            driver_version: "570.42.01".to_owned(),
            models: vec!["Fixture GPU".to_owned()],
            device_ids: vec!["GPU-a".to_owned()],
            memories: vec![Ok(dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 1_000,
                used_bytes: 400,
            })],
            processes: vec![Vec::new()],
            device_count_calls: Arc::new(AtomicUsize::new(0)),
            indexed_poll_calls: Arc::new(AtomicUsize::new(0)),
            uuid_poll_calls: Arc::new(AtomicUsize::new(0)),
        }))],
        Arc::new(FixedClock),
    );
    let host = HostSample {
        memory: Some(HostMemorySample {
            total_bytes: 68_719_476_736,
            available_bytes: 34_359_738_368,
            cached_bytes: Some(8_589_934_592),
        }),
        ..HostSample::default()
    };

    // Discovery belongs to the full snapshot, so a sample before it reads no accelerator.
    assert_eq!(
        collector.sample(),
        HardwareSample {
            host: host.clone(),
            accelerators: vec![],
        }
    );
    collector.collect_observation();
    assert_eq!(
        collector.sample(),
        HardwareSample {
            host,
            accelerators: vec![AcceleratorSample {
                memory_total_bytes: Some(1_000),
                memory_used_bytes: Some(400),
                ..AcceleratorSample::new("GPU-a".to_owned())
            }],
        }
    );
}

#[test]
fn a_sample_completes_while_a_snapshot_poll_is_blocked() {
    let (entered, entered_receiver) = std::sync::mpsc::channel();
    let (release, release_receiver) = std::sync::mpsc::channel();
    let collector = Arc::new(HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![Box::new(BlockingPollDiscoverer {
            poll: Mutex::new(Some((entered, release_receiver))),
        })],
        Arc::new(FixedClock),
    ));
    let snapshot = std::thread::spawn({
        let collector = Arc::clone(&collector);
        move || collector.collect_observation()
    });
    entered_receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the snapshot poll starts");

    let (sampled, sample_receiver) = std::sync::mpsc::channel();
    std::thread::spawn({
        let collector = Arc::clone(&collector);
        move || sampled.send(collector.sample())
    });
    let sample = sample_receiver.recv_timeout(std::time::Duration::from_secs(10));

    release.send(()).expect("release the poll");
    snapshot.join().expect("snapshot completes");
    let sample = sample.expect("the sample completes while the poll is blocked");
    assert_eq!(
        sample.accelerators,
        vec![AcceleratorSample {
            memory_total_bytes: Some(1_000),
            memory_used_bytes: Some(400),
            ..AcceleratorSample::new("GPU-a".to_owned())
        }]
    );
}

#[test]
fn amd_adapter_falls_back_to_kernel_release_without_the_amdgpu_module() {
    let (_dir, root) = amd_fixture();
    fs::remove_file(root.join("sys/module/amdgpu/version")).expect("remove module version");
    fs::create_dir_all(root.join("proc/sys/kernel")).expect("create kernel dir");
    fs::write(root.join("proc/sys/kernel/osrelease"), "6.12.9\n").expect("write osrelease");
    let adapter = SystemAmdAcceleratorAdapter::with_root(&root);

    let facts = adapter.collect_accelerator_facts().expect("AMD facts");

    assert!(
        facts
            .iter()
            .all(|fact| fact.driver_or_runtime_version == "6.12.9")
    );
}

#[test]
fn amd_adapter_ignores_non_amd_cards_and_reports_no_device_when_none_remain() {
    let (_dir, root) = amd_fixture();
    let card2 = root.join("sys/class/drm/card2/device");
    fs::create_dir_all(&card2).expect("create card2");
    fs::write(card2.join("vendor"), "0x8086\n").expect("write intel vendor");
    fs::remove_file(root.join("sys/class/drm/card0/device/vendor")).expect("remove amd card");
    fs::remove_file(root.join("sys/class/drm/card1/device/vendor")).expect("remove amd card");
    let adapter = SystemAmdAcceleratorAdapter::with_root(&root);

    assert_eq!(
        adapter
            .collect_accelerator_facts()
            .expect_err("non-AMD cards should not produce AMD facts")
            .reason(),
        UnavailableReason::NoDevice
    );
}

#[test]
fn amd_adapter_reports_no_device_when_sysfs_is_absent() {
    let dir = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
        .expect("temp sysfs fixture");
    let adapter = SystemAmdAcceleratorAdapter::with_root(dir.path());

    assert_eq!(
        adapter
            .collect_accelerator_facts()
            .expect_err("missing DRM sysfs should be unavailable")
            .reason(),
        UnavailableReason::NoDevice
    );
}

#[test]
fn registered_discoverer_runs_once_and_detected_poller_runs_each_collection() {
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let poll_calls = Arc::new(AtomicUsize::new(0));
    let collector = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![Box::new(CountingDiscoverer {
            vendor: AcceleratorVendor::Nvidia,
            discovery_calls: Arc::clone(&discovery_calls),
            poll_calls: Arc::clone(&poll_calls),
            discovery: Mutex::new(Some(Ok(nvidia_facts()))),
            panic: false,
        })],
        Arc::new(FixedClock),
    );

    let first = collector.collect_observation();
    let second = collector.collect_observation();

    assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(poll_calls.load(Ordering::SeqCst), 2);
    assert_eq!(first.snapshot.accelerators, second.snapshot.accelerators);
}

#[test]
fn every_registered_vendor_discovers_once_and_polls_each_collection() {
    let nvidia_discovery_calls = Arc::new(AtomicUsize::new(0));
    let nvidia_poll_calls = Arc::new(AtomicUsize::new(0));
    let amd_discovery_calls = Arc::new(AtomicUsize::new(0));
    let amd_poll_calls = Arc::new(AtomicUsize::new(0));
    let collector = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![
            Box::new(CountingDiscoverer {
                vendor: AcceleratorVendor::Nvidia,
                discovery_calls: Arc::clone(&nvidia_discovery_calls),
                poll_calls: Arc::clone(&nvidia_poll_calls),
                discovery: Mutex::new(Some(Ok(nvidia_facts()))),
                panic: false,
            }),
            Box::new(CountingDiscoverer {
                vendor: AcceleratorVendor::Amd,
                discovery_calls: Arc::clone(&amd_discovery_calls),
                poll_calls: Arc::clone(&amd_poll_calls),
                discovery: Mutex::new(Some(Ok(amd_facts()))),
                panic: false,
            }),
        ],
        Arc::new(FixedClock),
    );

    let first = collector.collect_observation();
    let second = collector.collect_observation();

    assert_eq!(nvidia_discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(amd_discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(nvidia_poll_calls.load(Ordering::SeqCst), 2);
    assert_eq!(amd_poll_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        first.snapshot.accelerators,
        vec![
            accelerator_observation(AcceleratorVendor::Nvidia, Ok(nvidia_facts())),
            accelerator_observation(AcceleratorVendor::Amd, Ok(amd_facts())),
        ]
    );
    assert_eq!(second.snapshot.accelerators, first.snapshot.accelerators);
}

#[test]
fn polling_task_failure_retains_discovery_without_rediscovery() {
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let poll_calls = Arc::new(AtomicUsize::new(0));
    let collector = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![Box::new(PanickingPollDiscoverer {
            discovery_calls: Arc::clone(&discovery_calls),
            poll_calls: Arc::clone(&poll_calls),
        })],
        Arc::new(FixedClock),
    );

    assert!(
        std::panic::catch_unwind(AssertUnwindSafe(|| collector.collect_observation())).is_err()
    );
    let recovered = collector.collect_observation();

    assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(poll_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        recovered.snapshot.accelerators[0].result,
        Some(accelerator_observation::Result::Facts(
            AcceleratorFactsSet {
                facts: nvidia_facts()
            }
        ))
    );
}

#[test]
fn transient_poll_failure_recovers_against_the_fixed_device_set() {
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let poll_calls = Arc::new(AtomicUsize::new(0));
    let collector = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![Box::new(RecoveringPollDiscoverer {
            discovery_calls: Arc::clone(&discovery_calls),
            poll_calls: Arc::clone(&poll_calls),
            facts: nvidia_facts(),
        })],
        Arc::new(FixedClock),
    );

    let failed = collector.collect_observation();
    let recovered = collector.collect_observation();

    assert_eq!(
        failed.snapshot.accelerators[0].result,
        Some(accelerator_observation::Result::Unavailable(
            ObservationUnavailable {
                code: "COLLECTION_FAILED".to_owned(),
                message: "NVIDIA observation is unavailable because collection failed.".to_owned(),
            }
        ))
    );
    assert_eq!(
        recovered.snapshot.accelerators[0].result,
        Some(accelerator_observation::Result::Facts(
            AcceleratorFactsSet {
                facts: nvidia_facts()
            }
        ))
    );
    assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(poll_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn absent_and_unavailable_discovery_are_fixed_without_pollers() {
    let cases = [
        (
            Ok(Vec::new()),
            accelerator_observation::Result::Facts(AcceleratorFactsSet { facts: Vec::new() }),
        ),
        (
            Err(UnavailableReason::MissingLibrary),
            accelerator_observation::Result::Unavailable(ObservationUnavailable {
                code: "MISSING_LIBRARY".to_owned(),
                message: "NVIDIA observation is unavailable because its library is not available."
                    .to_owned(),
            }),
        ),
    ];

    for (discovery, expected) in cases {
        let discovery_calls = Arc::new(AtomicUsize::new(0));
        let poll_calls = Arc::new(AtomicUsize::new(0));
        let collector = HardwareSnapshotCollector::new(
            FakeHost(Ok(host_facts())),
            vec![Box::new(CountingDiscoverer {
                vendor: AcceleratorVendor::Nvidia,
                discovery_calls: Arc::clone(&discovery_calls),
                poll_calls: Arc::clone(&poll_calls),
                discovery: Mutex::new(Some(discovery)),
                panic: false,
            })],
            Arc::new(FixedClock),
        );

        let first = collector.collect_observation();
        let second = collector.collect_observation();

        assert_eq!(
            first.snapshot.accelerators[0].result,
            Some(expected.clone())
        );
        assert_eq!(second.snapshot.accelerators[0].result, Some(expected));
        assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(poll_calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn unwinding_discoverer_becomes_unavailable_without_rediscovery() {
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let poll_calls = Arc::new(AtomicUsize::new(0));
    let collector = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![Box::new(CountingDiscoverer {
            vendor: AcceleratorVendor::Nvidia,
            discovery_calls: Arc::clone(&discovery_calls),
            poll_calls: Arc::clone(&poll_calls),
            discovery: Mutex::new(Some(Ok(nvidia_facts()))),
            panic: true,
        })],
        Arc::new(FixedClock),
    );

    let first = collector.collect_observation();
    let second = collector.collect_observation();

    for observation in [first, second] {
        assert_eq!(
            observation.snapshot.accelerators[0].result,
            Some(accelerator_observation::Result::Unavailable(
                ObservationUnavailable {
                    code: "COLLECTION_FAILED".to_owned(),
                    message: "NVIDIA observation is unavailable because collection failed."
                        .to_owned(),
                }
            ))
        );
    }
    assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(poll_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn returns_host_and_per_vendor_accelerator_facts_from_injected_adapters() {
    let snapshot = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![
            Box::new(FakeAccelerator {
                vendor: AcceleratorVendor::Nvidia,
                result: Ok(nvidia_facts()),
            }),
            Box::new(FakeAccelerator {
                vendor: AcceleratorVendor::Amd,
                result: Err(UnavailableReason::NoDevice),
            }),
        ],
        Arc::new(FixedClock),
    )
    .collect_snapshot();

    assert_eq!(
        snapshot.observed_at,
        Some(timestamp_from(FixedClock.wall_now()))
    );
    assert_eq!(
        snapshot.host.unwrap().result,
        Some(host_observation::Result::Facts(host_facts()))
    );
    assert_eq!(snapshot.accelerators.len(), 2);
    assert_eq!(
        snapshot.accelerators[0].vendor,
        AcceleratorVendor::Nvidia as i32
    );
    assert_eq!(
        snapshot.accelerators[0].result,
        Some(accelerator_observation::Result::Facts(
            AcceleratorFactsSet {
                facts: nvidia_facts()
            }
        ))
    );
    assert_eq!(
        snapshot.accelerators[1].vendor,
        AcceleratorVendor::Amd as i32
    );
    assert_eq!(
        snapshot.accelerators[1].result,
        Some(accelerator_observation::Result::Unavailable(
            ObservationUnavailable {
                code: "NO_DEVICE".to_owned(),
                message: "AMD observation is unavailable because no device is present.".to_owned(),
            }
        ))
    );
}

#[test]
fn reports_stable_unavailability_codes_for_each_expected_reason() {
    let cases = [
        (
            UnavailableReason::Unsupported,
            "UNSUPPORTED",
            "NVIDIA observation is unavailable because this host does not support it.",
        ),
        (
            UnavailableReason::PermissionDenied,
            "PERMISSION_DENIED",
            "NVIDIA observation is unavailable because access was denied.",
        ),
        (
            UnavailableReason::MissingLibrary,
            "MISSING_LIBRARY",
            "NVIDIA observation is unavailable because its library is not available.",
        ),
        (
            UnavailableReason::NoDevice,
            "NO_DEVICE",
            "NVIDIA observation is unavailable because no device is present.",
        ),
    ];

    for (reason, code, message) in cases {
        let snapshot = HardwareSnapshotCollector::new(
            FakeHost(Ok(host_facts())),
            vec![Box::new(FakeAccelerator {
                vendor: AcceleratorVendor::Nvidia,
                result: Err(reason),
            })],
            Arc::new(FixedClock),
        )
        .collect_snapshot();
        let Some(accelerator_observation::Result::Unavailable(unavailable)) =
            &snapshot.accelerators[0].result
        else {
            panic!("NVIDIA result must be unavailable");
        };
        assert_eq!(unavailable.code.as_str(), code);
        assert_eq!(unavailable.message.as_str(), message);
    }
}

#[test]
fn reports_host_failure_without_false_zero_facts() {
    let snapshot = HardwareSnapshotCollector::new(
        FakeHost(Err(UnavailableReason::CollectionFailed)),
        vec![Box::new(FakeAccelerator {
            vendor: AcceleratorVendor::Nvidia,
            result: Ok(nvidia_facts()),
        })],
        Arc::new(FixedClock),
    )
    .collect_snapshot();

    assert_eq!(
        snapshot.host.unwrap().result,
        Some(host_observation::Result::Unavailable(
            ObservationUnavailable {
                code: "COLLECTION_FAILED".to_owned(),
                message: "Host observation is unavailable because collection failed.".to_owned(),
            }
        ))
    );
    assert_eq!(
        snapshot.accelerators[0].result,
        Some(accelerator_observation::Result::Facts(
            AcceleratorFactsSet {
                facts: nvidia_facts()
            }
        ))
    );
}

#[test]
fn a_host_without_accelerators_still_returns_a_successful_snapshot() {
    let snapshot = HardwareSnapshotCollector::new(
        FakeHost(Ok(host_facts())),
        vec![
            Box::new(FakeAccelerator {
                vendor: AcceleratorVendor::Nvidia,
                result: Err(UnavailableReason::MissingLibrary),
            }),
            Box::new(FakeAccelerator {
                vendor: AcceleratorVendor::Amd,
                result: Ok(Vec::new()),
            }),
        ],
        Arc::new(FixedClock),
    )
    .collect_snapshot();

    assert_eq!(snapshot.accelerators.len(), 2);
    assert!(matches!(
        snapshot.accelerators[0].result,
        Some(accelerator_observation::Result::Unavailable(_))
    ));
    assert_eq!(
        snapshot.accelerators[1].result,
        Some(accelerator_observation::Result::Facts(
            AcceleratorFactsSet { facts: Vec::new() }
        ))
    );
}

#[test]
fn amd_adapter_matches_the_live_host_sysfs_inventory() {
    // Cross-checks the adapter against an independent enumeration of the
    // live host, so the test is deterministic on AMD and non-AMD hosts.
    let adapter = SystemAmdAcceleratorAdapter::for_current_host();
    let expected_models = live_amd_drm_card_models(Path::new("/"));
    match adapter.collect_accelerator_facts() {
        Err(error) if error.reason() == UnavailableReason::NoDevice => {
            assert!(expected_models.is_empty());
        }
        Ok(facts) => {
            assert!(!expected_models.is_empty());
            assert_eq!(facts.len(), expected_models.len());
            for fact in &facts {
                assert_eq!(fact.vendor, AcceleratorVendor::Amd as i32);
                assert!(expected_models.iter().any(|model| model == &fact.model));
                assert!(!fact.driver_or_runtime_version.is_empty());
            }
        }
        other => panic!("unexpected AMD inventory outcome: {other:?}"),
    }
}

#[test]
fn apple_is_declared_in_the_contract_without_an_adapter() {
    // Apple stays a declared vendor family in the contract; this phase
    // ships no Apple adapter, so the value exists only for shape.
    assert_eq!(AcceleratorVendor::Apple as i32, 3);
    assert_eq!(
        AcceleratorVendor::Apple.as_str_name(),
        "ACCELERATOR_VENDOR_APPLE"
    );
}

/// The models an independent enumeration of the live host reports for AMD
/// DRM cards, using the same sysfs layout the adapter reads.
fn live_amd_drm_card_models(root: &Path) -> Vec<String> {
    let drm_root = root.join("sys").join("class").join("drm");
    let Ok(cards) = fs::read_dir(drm_root) else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for entry in cards.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with("card") {
            continue;
        }
        let device_root = entry.path().join("device");
        // Match the adapter's card-versus-connector discrimination.
        let Ok(device_meta) = fs::symlink_metadata(&device_root) else {
            continue;
        };
        if !device_meta.is_dir() {
            continue;
        }
        let Ok(Some(vendor)) = pci_id(&device_root.join("vendor")) else {
            continue;
        };
        if vendor != "1002" {
            continue;
        }
        let Ok(Some(device)) = pci_id(&device_root.join("device")) else {
            continue;
        };
        models.push(format!("{vendor}:{device}"));
    }
    models
}

#[test]
fn unavailable_accelerators_record_component_warnings_without_failing_the_span() {
    let (_, records, output) = capture_records(|| {
        HardwareSnapshotCollector::new(
            FakeHost(Ok(host_facts())),
            vec![Box::new(FakeAccelerator {
                vendor: AcceleratorVendor::Nvidia,
                result: Err(UnavailableReason::MissingLibrary),
            })],
            Arc::new(FixedClock),
        )
        .collect_snapshot();
    });

    assert!(records.iter().any(|record| matches!(
        record,
        Record::SpanClosed { name, fields }
            if name == "hardware_observation"
                && fields.get("outcome").is_some_and(|value| value.contains("success"))
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Event { level, fields }
            if level == "WARN"
                && fields.get("component").is_some_and(|value| value.contains("nvidia"))
                && fields.get("error_kind").is_some_and(|value| value.contains("MissingLibrary"))
    )));
    assert!(!output.contains("credentials") && !output.contains("prompt"));
}

fn host_facts() -> HostFacts {
    HostFacts {
        host_name: "node-1".to_owned(),
        operating_system_or_kernel: "NixOS 26.05".to_owned(),
        cpu_count: 16,
        total_memory_bytes: 68_719_476_736,
        available_memory_bytes: 34_359_738_368,
        cached_memory_bytes: Some(8_589_934_592),
        filesystems: vec![FilesystemFacts {
            mountpoint: "/".to_owned(),
            fstype: "ext4".to_owned(),
            size_bytes: 1_099_511_627_776,
        }],
    }
}

fn nvidia_facts() -> Vec<AcceleratorFacts> {
    vec![AcceleratorFacts {
        vendor: AcceleratorVendor::Nvidia as i32,
        model: "NVIDIA RTX PRO 6000 Blackwell".to_owned(),
        memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
        total_memory_bytes: Some(100_000_000_000),
        driver_or_runtime_version: "570.42.01".to_owned(),
        device_id: String::new(),
        vendor_index: None,
        used_memory_bytes: None,
        working_set_limit_bytes: None,
        supported_signals: Vec::new(),
    }]
}

fn amd_facts() -> Vec<AcceleratorFacts> {
    vec![AcceleratorFacts {
        vendor: AcceleratorVendor::Amd as i32,
        model: "Fixture AMD GPU".to_owned(),
        memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
        total_memory_bytes: Some(16_000),
        driver_or_runtime_version: "6.9.0".to_owned(),
        device_id: "AMD-fixture".to_owned(),
        vendor_index: None,
        used_memory_bytes: Some(4_000),
        working_set_limit_bytes: None,
        supported_signals: Vec::new(),
    }]
}
