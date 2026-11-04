//! Builds the workload resource telemetry record for one development workload VRAM
//! measurement, pairing the launch inputs kept with the workload's ownership
//! handle with the measured peak accelerator memory, the peak memory of other processes
//! sharing the device, and any engine startup facts.
//!
//! The record names the measuring host instead of a site node, since development
//! workloads run outside any site, and has no time since readiness.

use std::time::SystemTime;

use dotcore_node_hardware::vram::{VramGpuMemory, VramObservation};
use dotcore_proto::dotcore::{
    observation::v1::AcceleratorMemoryArchitecture,
    telemetry::v1::{
        WorkloadRealizationKey, WorkloadResourceDevice, WorkloadResourceNode,
        WorkloadResourceObservation, WorkloadResourceObservationKind, WorkloadResourceRecord,
        WorkloadResourceSource, workload_resource_observation::EngineFacts,
    },
};
use sysinfo::System;

use super::DevelopmentWorkloadProcessIdentity;

/// Builds the telemetry record of one development VRAM measurement on this host.
pub fn measurement_record(
    identity: &DevelopmentWorkloadProcessIdentity,
    observation: &VramObservation,
    engine_facts: Option<EngineFacts>,
    recorded_at: SystemTime,
) -> WorkloadResourceRecord {
    host_measurement_record(
        System::host_name().unwrap_or_default(),
        identity,
        observation,
        engine_facts,
        recorded_at,
    )
}

fn host_measurement_record(
    host_name: String,
    identity: &DevelopmentWorkloadProcessIdentity,
    observation: &VramObservation,
    engine_facts: Option<EngineFacts>,
    recorded_at: SystemTime,
) -> WorkloadResourceRecord {
    let (architecture, device_total, host_total, host_available) = match observation.gpu.memory {
        VramGpuMemory::Dedicated { total_bytes } => (
            AcceleratorMemoryArchitecture::Dedicated,
            Some(total_bytes),
            None,
            None,
        ),
        VramGpuMemory::Unified {
            host_total_bytes,
            host_available_bytes,
        } => (
            AcceleratorMemoryArchitecture::Unified,
            None,
            Some(host_total_bytes),
            Some(host_available_bytes),
        ),
    };
    WorkloadResourceRecord {
        realization: Some(WorkloadRealizationKey {
            owner_id: identity.owner_id.clone(),
            generation: identity.generation,
        }),
        inputs: Some(identity.launch_inputs.clone()),
        node: Some(WorkloadResourceNode {
            node_id: host_name,
            host_total_memory_bytes: host_total,
            host_available_memory_bytes: host_available,
            devices: vec![WorkloadResourceDevice {
                device_id: observation.gpu.uuid.clone(),
                model: observation.gpu.name.clone(),
                driver_version: observation.gpu.driver_version.clone(),
                memory_architecture: architecture.into(),
                total_memory_bytes: device_total,
                attributed_memory_bytes: observation.observed_max_bytes,
                unattributed_peak_memory_bytes: Some(observation.unattributed_max_bytes),
            }],
        }),
        observation: Some(WorkloadResourceObservation {
            kind: WorkloadResourceObservationKind::Measurement.into(),
            since_ready: None,
            attributed_memory_bytes: observation.observed_max_bytes,
            engine_facts,
        }),
        ..crate::telemetry::new_workload_resource_record(
            WorkloadResourceSource::Development,
            recorded_at,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use dotcore_node_hardware::vram::{VramGpuIdentity, VramOtherProcess};
    use dotcore_proto::dotcore::telemetry::v1::{VllmStartupFacts, WorkloadResourceInputs};
    use dotcore_runtime::WorkloadId;

    use super::*;

    #[test]
    fn unified_measurement_records_host_memory_and_engine_facts() {
        let identity = identity();
        let facts = EngineFacts::Vllm(VllmStartupFacts {
            extractor_version: 1,
            kv_cache_token_capacity: Some(172_480),
            ..VllmStartupFacts::default()
        });

        let record = host_measurement_record(
            "spark".to_owned(),
            &identity,
            &observation(VramGpuMemory::Unified {
                host_total_bytes: 130_000_000_000,
                host_available_bytes: 90_000_000_000,
            }),
            Some(facts),
            UNIX_EPOCH + Duration::from_secs(10),
        );

        assert_eq!(record.source, WorkloadResourceSource::Development as i32);
        assert_eq!(
            record.realization,
            Some(WorkloadRealizationKey {
                owner_id: "runner-owner".to_owned(),
                generation: 7,
            })
        );
        assert_eq!(record.inputs, Some(identity.launch_inputs));
        assert_eq!(
            record.node,
            Some(WorkloadResourceNode {
                node_id: "spark".to_owned(),
                host_total_memory_bytes: Some(130_000_000_000),
                host_available_memory_bytes: Some(90_000_000_000),
                devices: vec![WorkloadResourceDevice {
                    device_id: "GPU-1".to_owned(),
                    model: "NVIDIA GB10".to_owned(),
                    driver_version: "580.95".to_owned(),
                    memory_architecture: AcceleratorMemoryArchitecture::Unified.into(),
                    total_memory_bytes: None,
                    attributed_memory_bytes: 30_000,
                    unattributed_peak_memory_bytes: Some(4_000),
                }],
            })
        );
        assert_eq!(
            record.observation,
            Some(WorkloadResourceObservation {
                kind: WorkloadResourceObservationKind::Measurement.into(),
                since_ready: None,
                attributed_memory_bytes: 30_000,
                engine_facts: Some(facts),
            })
        );
    }

    #[test]
    fn dedicated_measurement_records_device_total() {
        let record = host_measurement_record(
            "gpu1".to_owned(),
            &identity(),
            &observation(VramGpuMemory::Dedicated {
                total_bytes: 32_000,
            }),
            None,
            UNIX_EPOCH,
        );

        let node = record.node.expect("node");
        assert_eq!(node.host_total_memory_bytes, None);
        assert_eq!(node.devices[0].total_memory_bytes, Some(32_000));
        assert_eq!(
            node.devices[0].memory_architecture,
            AcceleratorMemoryArchitecture::Dedicated as i32
        );
        assert_eq!(record.observation.expect("observation").engine_facts, None);
    }

    fn identity() -> DevelopmentWorkloadProcessIdentity {
        DevelopmentWorkloadProcessIdentity {
            runtime_scope: "development-scope".to_owned(),
            runtime_backend_id: "runtime/docker".to_owned(),
            owner_id: "runner-owner".to_owned(),
            generation: 7,
            launch_fingerprint: "a".repeat(64),
            workload_id: WorkloadId("workload".to_owned()),
            runtime_process_id: "container".to_owned(),
            process_id: 42,
            launch_inputs: WorkloadResourceInputs {
                model_id: "qwen3-8-27b".to_owned(),
                engine_release_id: "vllm".to_owned(),
                ..WorkloadResourceInputs::default()
            },
        }
    }

    fn observation(memory: VramGpuMemory) -> VramObservation {
        VramObservation {
            process_id: 42,
            process_start_token: 1,
            started_unix_millis: 0,
            duration_millis: 1_000,
            interval_millis: 25,
            gpu: VramGpuIdentity {
                index: 0,
                uuid: "GPU-1".to_owned(),
                name: "NVIDIA GB10".to_owned(),
                driver_version: "580.95".to_owned(),
                cuda_driver_version: 13_000,
                memory,
            },
            isolated: false,
            samples: Vec::new(),
            observed_max_bytes: 30_000,
            unattributed_max_bytes: 4_000,
            processes: Vec::new(),
            other_processes: vec![VramOtherProcess {
                process_id: 99,
                memory_known: true,
            }],
        }
    }
}
