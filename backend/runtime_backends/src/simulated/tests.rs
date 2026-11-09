//! Simulated runtime lifecycle characterization tests.
use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
};

use dotcore_domain::SiteNodeId;
use dotcore_runtime::{
    ExecutionTarget, ObservedWorkload, RuntimeBackend, RuntimeBackendError, RuntimeBackendId,
    RuntimeLogEntry, RuntimeLogEvent, RuntimeLogFollowQuery, RuntimeLogQuery, RuntimeLogSource,
    RuntimeLogSourceSelection, RuntimeLogStreamKind, RuntimeWorkloadLogFollowQuery,
    RuntimeWorkloadLogQuery, WorkloadCorrelation, WorkloadGroupBackend, WorkloadGroupId,
    WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadId, WorkloadIpcMode, WorkloadLaunch,
    WorkloadObservation, WorkloadPublicationRequest, WorkloadStorageAttachment, WorkloadStorageKey,
    WorkloadStorageKind, WorkloadUnitLaunch, WorkloadUnitName, WorkloadUnitResources,
    WorkloadUnitResourcesUnavailable, WorkloadUnitState,
};
use futures_util::StreamExt as _;

use super::{
    BACKEND_ID, InMemoryRuntimeBackend, SimulatedRuntimeCallCounts, SimulatedRuntimeTestSupport,
    adapter::{FakeWorkload, FakeWorkloadState},
    configuration::SimulatedRuntimeConfig,
};

#[tokio::test]
async fn retained_workload_platform_mismatch_is_rejected_without_runtime_substitution() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_arm64());
    let error = runtime
        .create_workload(&individual_workload_launch())
        .await
        .expect_err("amd64 workload must not launch on arm64 runtime");

    assert!(matches!(
        error,
        RuntimeBackendError::TargetPlatformMismatch {
            required_operating_system,
            required_architecture,
            observed_operating_system,
            observed_architecture,
        } if required_operating_system == "linux"
            && required_architecture == "amd64"
            && observed_operating_system == "linux"
            && observed_architecture == "arm64"
    ));
    assert_eq!(
        runtime
            .discover_workloads()
            .await
            .expect("Runtime discovery"),
        Vec::new()
    );
}

#[tokio::test]
async fn bounded_stop_failures_preserve_workload_until_a_later_stop() {
    let test_support = SimulatedRuntimeTestSupport::with_stop_failures(1);
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        test_support: Some(test_support.clone()),
        ..Default::default()
    });
    let created = runtime
        .create_workload(&individual_workload_launch())
        .await
        .expect("create simulated workload");
    let workload_id = match created {
        WorkloadObservation::Running(workload) => workload.workload_id,
        _ => panic!("zero-delay workload must be running"),
    };

    assert!(runtime.stop_workload(&workload_id).await.is_err());
    assert!(matches!(
        runtime.inspect_workload(&workload_id).await,
        Ok(WorkloadObservation::Running(_))
    ));

    runtime
        .stop_workload(&workload_id)
        .await
        .expect("bounded stop retry");
    assert!(matches!(
        runtime.inspect_workload(&workload_id).await,
        Ok(WorkloadObservation::Stopped { .. })
    ));
    assert_eq!(
        test_support.call_counts(),
        SimulatedRuntimeCallCounts {
            creates: 1,
            starts: 0,
            stops: 2,
            failed_stops: 1,
        }
    );
}

#[tokio::test]
async fn a_model_workload_takes_its_expected_memory_on_the_declared_machine_until_removed() {
    use dotcore_domain::hardware_observation::{
        AcceleratorProcessObservation, AcceleratorVendorProcessResult, HardwareSnapshotSource,
    };

    const GPU: &str = "GPU-00000000-0000-0000-0000-000000005090";
    let machine = super::DeclaredMachine::new(
        dotcore_node_hardware::MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
            .expect("named machine"),
        std::sync::Arc::new(dotcore_domain::clock::SystemClock),
    );
    let node = SiteNodeId::for_test("node-01");
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        managed_node_id: Some(node.clone()),
        machine: Some(machine.clone()),
        ..Default::default()
    });
    let mut launch = individual_workload_launch();
    launch.spec.expected_memory = dotcore_runtime::WorkloadExpectedMemory {
        device_bytes: BTreeMap::from([(GPU.to_owned(), 7_000)]),
        host_bytes: 11,
    };
    let rows = |machine: &super::DeclaredMachine| match machine
        .collect_observation()
        .accelerator_processes
    {
        AcceleratorProcessObservation::Observed(vendors) => vendors
            .into_iter()
            .flat_map(|vendor| match vendor.result {
                AcceleratorVendorProcessResult::Observed(rows) => rows,
                AcceleratorVendorProcessResult::Unavailable(reason) => {
                    panic!("unavailable vendor rows: {reason:?}")
                }
            })
            .map(|row| (row.device_id, row.process_id, row.used_memory_bytes))
            .collect::<Vec<_>>(),
        AcceleratorProcessObservation::Unavailable(reason) => {
            panic!("unavailable process rows: {reason:?}")
        }
    };

    let created = runtime.create_workload(&launch).await.expect("create");
    let WorkloadObservation::Running(observed) = created else {
        panic!("a created workload with no ready delay runs: {created:?}");
    };
    let process = observed.host_process.clone().expect("simulated process");
    assert_eq!(process.managed_node_id, node);
    assert_eq!(
        rows(&machine),
        [(GPU.to_owned(), process.process_id, Some(7_000))]
    );

    runtime
        .remove_workload(&observed.workload_id)
        .await
        .expect("remove");
    assert_eq!(rows(&machine), []);

    // A workload holding only host memory takes it on the machine but is no accelerator process.
    let idle = machine.collect_snapshot();
    let mut host_only = individual_workload_launch();
    host_only.correlation.owner_id = "deployment-02".to_owned();
    host_only.spec.expected_memory.host_bytes = 11;
    let created = runtime.create_workload(&host_only).await.expect("create");
    let WorkloadObservation::Running(observed) = created else {
        panic!("a created workload with no ready delay runs: {created:?}");
    };
    assert_eq!(observed.host_process, None);
    assert_ne!(machine.collect_snapshot().host, idle.host);
    runtime
        .remove_workload(&observed.workload_id)
        .await
        .expect("remove");
    assert_eq!(machine.collect_snapshot().host, idle.host);
}

#[tokio::test]
async fn a_workload_on_a_gpu_the_declared_machine_lacks_is_refused() {
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        managed_node_id: Some(SiteNodeId::for_test("node-01")),
        machine: Some(super::DeclaredMachine::new(
            dotcore_node_hardware::MachineDescription::named("gb10").expect("named machine"),
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        )),
        ..Default::default()
    });
    let mut launch = individual_workload_launch();
    launch.spec.expected_memory.device_bytes = BTreeMap::from([("GPU-missing".to_owned(), 1)]);

    let error = runtime
        .create_workload(&launch)
        .await
        .expect_err("unknown GPU");

    assert!(
        matches!(error, RuntimeBackendError::InvalidLaunch(_)),
        "{error:?}"
    );
    assert_eq!(
        runtime.discover_workloads().await.expect("discovery"),
        Vec::new()
    );
}

#[tokio::test]
async fn accepts_host_shared_ipc_workloads() {
    let test_support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        test_support: Some(test_support.clone()),
        ..Default::default()
    });
    let mut workload = individual_workload_launch();
    workload.spec.ipc_mode = WorkloadIpcMode::HostShared;

    runtime
        .create_workload(&workload)
        .await
        .expect("simulated runtime accepts host-shared IPC");

    assert_eq!(
        test_support.call_counts(),
        SimulatedRuntimeCallCounts {
            creates: 1,
            starts: 0,
            stops: 0,
            failed_stops: 0,
        }
    );
}

#[tokio::test]
async fn supports_group_creation_inspection_and_removal_without_changing_individual_workloads() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let group = group_launch();
    assert!(matches!(
        runtime.create_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Observed(_))
    ));
    assert!(matches!(
        runtime.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Observed(_))
    ));
    runtime
        .remove_workload_group(&group)
        .await
        .expect("remove group");
    runtime
        .remove_workload_group(&group)
        .await
        .expect("repeat absent removal");
    assert!(matches!(
        runtime.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Absent { .. })
    ));
    assert!(
        runtime
            .discover_workloads()
            .await
            .expect("individual discovery")
            .is_empty()
    );
}

#[tokio::test]
async fn stable_identity_group_cleanup_is_owner_checked_idempotent_and_preserves_data() {
    let support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        test_support: Some(support.clone()),
        ..Default::default()
    });
    let group = group_launch();
    runtime
        .create_workload_group(&group)
        .await
        .expect("create simulated group");
    assert!(support.storage_exists(&group_storage(&group)));

    assert!(
        runtime
            .ensure_workload_group_absent(&group.group_id, "another-owner")
            .await
            .is_err()
    );
    assert!(matches!(
        runtime.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Observed(_))
    ));

    runtime
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("stable identity cleanup");
    runtime
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("repeated stable identity cleanup");
    assert!(matches!(
        runtime.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Absent { .. })
    ));
    assert!(support.storage_exists(&group_storage(&group)));
    assert_eq!(
        support
            .group_calls()
            .iter()
            .filter(|call| call.operation == "ensure-absent")
            .count(),
        2
    );
}

#[tokio::test]
async fn group_removal_rejects_mismatched_launch_without_mutation() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let group = group_launch();
    runtime
        .create_workload_group(&group)
        .await
        .expect("create group");
    let mut mismatched = group.clone();
    mismatched.correlation.reconcile_generation += 1;

    assert!(runtime.remove_workload_group(&mismatched).await.is_err());
    assert!(matches!(
        runtime.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Observed(_))
    ));
}

#[tokio::test]
async fn observed_group_removal_accepts_the_retained_launch_identity() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let group = group_launch();
    runtime
        .create_workload_group(&group)
        .await
        .expect("create group");
    let WorkloadGroupObservation::Observed(observed) = runtime
        .inspect_workload_group(&group)
        .await
        .expect("inspect group")
    else {
        panic!("created group must be observed")
    };

    runtime
        .remove_observed_workload_group(&observed)
        .await
        .expect("remove observed group");
    assert!(matches!(
        runtime.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Absent { .. })
    ));
}

#[tokio::test]
async fn group_stop_and_start_retain_identity_without_removal() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let group = group_launch();
    runtime
        .create_workload_group(&group)
        .await
        .expect("create group");

    runtime
        .stop_workload_group(&group)
        .await
        .expect("stop retained group");
    let WorkloadGroupObservation::Observed(stopped) = runtime
        .inspect_workload_group(&group)
        .await
        .expect("inspect stopped group")
    else {
        panic!("retained group must remain observed")
    };
    assert!(
        stopped
            .units
            .values()
            .all(|unit| unit.state == WorkloadUnitState::Stopped)
    );

    let WorkloadGroupObservation::Observed(started) = runtime
        .start_workload_group(&group)
        .await
        .expect("start retained group")
    else {
        panic!("retained group must remain observed")
    };
    assert_eq!(started.group_id, group.group_id);
    assert_eq!(started.correlation, group.correlation);
    assert!(
        started
            .units
            .values()
            .all(|unit| unit.state == WorkloadUnitState::Running)
    );
}

#[tokio::test]
async fn running_group_units_take_synthetic_host_memory_and_cpu_time_on_the_declared_machine() {
    use dotcore_domain::hardware_observation::HardwareSnapshotSource;
    use dotcore_proto::dotcore::observation::v1::host_observation;

    let clock = dotcore_domain::clock::VirtualClock::starting_at(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
    );
    let machine = super::DeclaredMachine::new(
        dotcore_node_hardware::MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
            .expect("named machine"),
        std::sync::Arc::new(clock.clone()),
    );
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        managed_node_id: Some(SiteNodeId::for_test("node-01")),
        machine: Some(machine.clone()),
        ..Default::default()
    });
    let group = group_launch();
    let unit_bytes = machine.description().host_memory_bytes() / 256;
    let available = || match machine.collect_snapshot().host.and_then(|host| host.result) {
        Some(host_observation::Result::Facts(facts)) => facts.available_memory_bytes,
        other => panic!("expected host facts, got {other:?}"),
    };
    let idle = available();

    runtime
        .create_workload_group(&group)
        .await
        .expect("create group");
    assert_eq!(available(), idle - unit_bytes);
    assert_eq!(
        web_resources(&runtime, &group, machine.wall_now()).await,
        Some((0, unit_bytes))
    );
    // A unit keeps a quarter of one CPU busy, so its CPU time grows by a quarter of wall time.
    clock.advance(std::time::Duration::from_secs(4));
    assert_eq!(
        web_resources(&runtime, &group, machine.wall_now()).await,
        Some((1_000_000_000, unit_bytes))
    );
    clock.advance(std::time::Duration::from_secs(2));
    assert_eq!(
        web_resources(&runtime, &group, machine.wall_now()).await,
        Some((1_500_000_000, unit_bytes))
    );

    runtime
        .stop_workload_group(&group)
        .await
        .expect("stop group");
    assert_eq!(available(), idle);
    assert_eq!(
        web_resources(&runtime, &group, machine.wall_now()).await,
        None
    );

    // A restarted unit takes its memory again and counts CPU time from its new start.
    runtime
        .start_workload_group(&group)
        .await
        .expect("start group");
    clock.advance(std::time::Duration::from_secs(4));
    assert_eq!(available(), idle - unit_bytes);
    assert_eq!(
        web_resources(&runtime, &group, machine.wall_now()).await,
        Some((1_000_000_000, unit_bytes))
    );

    runtime
        .remove_workload_group(&group)
        .await
        .expect("remove group");
    assert_eq!(available(), idle);
    assert_eq!(
        web_resources(&runtime, &group, machine.wall_now()).await,
        None
    );
}

#[tokio::test]
async fn group_units_without_a_declared_machine_report_no_resources() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let group = group_launch();
    runtime
        .create_workload_group(&group)
        .await
        .expect("create group");

    let observations = runtime
        .observe_group_resources(&group)
        .await
        .expect("observe group resources");

    assert_eq!(
        observations.keys().cloned().collect::<Vec<_>>(),
        [WorkloadUnitName("web".to_owned())]
    );
    assert!(
        observations
            .values()
            .all(|unit| matches!(unit.resources, WorkloadUnitResources::NotReported))
    );
}

#[tokio::test]
async fn device_carrying_group_launch_is_accepted_and_retained_without_realization() {
    let support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let runtime = InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
        test_support: Some(support.clone()),
        ..Default::default()
    });
    let mut group = group_launch();
    group.execution_target.accelerator_device_ids =
        vec!["nvidia.com/gpu=0".to_owned(), "nvidia.com/gpu=1".to_owned()];

    let WorkloadGroupObservation::Observed(observed) = runtime
        .create_workload_group(&group)
        .await
        .expect("create group carrying accelerator devices")
    else {
        panic!("a device-carrying group launch must be accepted")
    };
    assert!(
        observed
            .units
            .values()
            .all(|unit| unit.state == WorkloadUnitState::Running)
    );

    // The simulated runtime retains the launch without realizing its accelerator devices.
    let retained = support.group_launches();
    assert_eq!(retained.len(), 1);
    assert_eq!(
        retained[0].execution_target.accelerator_device_ids,
        group.execution_target.accelerator_device_ids
    );
}

/// The figures of the `web` unit of `group`, observed at `now`, or none while it is not running.
async fn web_resources(
    runtime: &InMemoryRuntimeBackend,
    group: &WorkloadGroupLaunch,
    now: std::time::SystemTime,
) -> Option<(u64, u64)> {
    let mut observations = runtime
        .observe_group_resources(group)
        .await
        .expect("observe group resources");
    assert_eq!(observations.len(), 1);
    let observation = observations
        .remove(&WorkloadUnitName("web".to_owned()))
        .expect("web unit");
    assert_eq!(observation.observed_at, now);
    match observation.resources {
        WorkloadUnitResources::Available {
            cpu_time_ns,
            memory_bytes,
        } => Some((cpu_time_ns, memory_bytes)),
        WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::NotRunning) => None,
        other => panic!("unexpected unit resources: {other:?}"),
    }
}

fn group_storage(group: &WorkloadGroupLaunch) -> WorkloadStorageKey {
    group.units.values().next().expect("unit").storage[0].storage_key()
}

fn group_launch() -> WorkloadGroupLaunch {
    WorkloadGroupLaunch {
        group_id: WorkloadGroupId("application-01".to_owned()),
        correlation: dotcore_runtime::WorkloadCorrelation {
            owner_id: "deployment-01".to_owned(),
            reconcile_generation: 1,
        },
        execution_target: ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-01"),
            accelerator_device_ids: Vec::new(),
        },
        execution_target_id: "application-target-01".to_owned(),
        required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
        units: BTreeMap::from([(
            WorkloadUnitName("web".to_owned()),
            WorkloadUnitLaunch {
                image: "example.test/web@sha256:abc".to_owned(),
                command: vec!["web".to_owned()],
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                reconfigurable_environment: BTreeSet::new(),
                storage: vec![WorkloadStorageAttachment {
                    name: "web-data".to_owned(),
                    destination: "/data".to_owned(),
                    kind: WorkloadStorageKind::Data,
                    namespace: "deployment".to_owned(),
                    key: "application-01".to_owned(),
                    read_only: false,
                    expires_when_unused: false,
                }],
                ports: Vec::new(),
                dependencies: Vec::new(),
                ready_launch_gate: None,
            },
        )]),
        publication: WorkloadPublicationRequest {
            unit_name: WorkloadUnitName("web".to_owned()),
            port_name: "http".to_owned(),
            configured_host: Ipv4Addr::LOCALHOST,
            host_port: std::num::NonZeroU16::new(30_000).expect("nonzero port"),
        },
        host_aliases: Default::default(),
    }
}

fn individual_workload_launch() -> WorkloadLaunch {
    WorkloadLaunch {
        correlation: WorkloadCorrelation {
            owner_id: "deployment-01".to_owned(),
            reconcile_generation: 1,
        },
        execution_target: ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-01"),
            accelerator_device_ids: Vec::new(),
        },
        spec: dotcore_runtime::WorkloadSpec {
            executable: dotcore_runtime::WorkloadExecutable {
                artifact: dotcore_runtime::WorkloadExecutableArtifact::OciImage(
                    "example.test/workload@sha256:abc".to_owned(),
                ),
                path: String::new(),
            },
            required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            artifacts: Vec::new(),
            storage: Vec::new(),
            ports: Vec::new(),
            accelerator_device_ids: Vec::new(),
            required_accelerator_device_count: 0,
            ipc_mode: dotcore_runtime::WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        },
    }
}

fn workload(id: &str) -> FakeWorkload {
    FakeWorkload {
        identity: ObservedWorkload {
            workload_id: WorkloadId(id.to_owned()),
            correlation: dotcore_runtime::WorkloadCorrelation {
                owner_id: format!("deployment-{id}"),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId(BACKEND_ID.to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        },
        machine_process: None,
        transition_at: tokio::time::Instant::now(),
        state: FakeWorkloadState::Stopped,
    }
}

#[tokio::test]
async fn simulated_runtime_direct_diagnostics_are_explicitly_unavailable() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let query = RuntimeWorkloadLogQuery::new(
        RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        },
        RuntimeLogQuery::new(
            std::num::NonZeroU16::new(1).expect("non-zero entry limit"),
            None,
        )
        .expect("bounded query"),
    );
    let individual = runtime
        .read_workload_logs(&WorkloadId("workload".to_owned()), &query)
        .await
        .expect_err("Simulated runtime must not fabricate direct diagnostics");

    assert!(matches!(
        individual,
        RuntimeBackendError::LogCapabilityUnavailable
    ));
}

#[tokio::test]
async fn simulated_group_logs_serve_each_unit_of_only_the_exact_retained_group() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    let web = RuntimeLogSource {
        name: "web".to_owned(),
        label: "web".to_owned(),
    };
    let selection = RuntimeLogSourceSelection::new(vec![web.clone()]).expect("selection");
    let query = |before| {
        RuntimeLogQuery::new(std::num::NonZeroU16::new(3).expect("limit"), before)
            .expect("bounded query")
    };
    let line = |number: u64| {
        format!("simulated workload log line {number:03} owner=deployment-01 generation=1 unit=web")
            .into_bytes()
    };
    let at = |number: u64| {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + 2 * number)
    };

    let before_create = runtime
        .read_workload_group_logs(&group_launch(), &selection, &query(None))
        .await
        .expect("an absent group reads per source");
    runtime
        .create_workload_group(&group_launch())
        .await
        .expect("create group");
    let newest = runtime
        .read_workload_group_logs(&group_launch(), &selection, &query(None))
        .await
        .expect("history");
    let older = runtime
        .read_workload_group_logs(&group_launch(), &selection, &query(Some(at(10))))
        .await
        .expect("older history");
    let mut other_generation = group_launch();
    other_generation.correlation.reconcile_generation = 2;
    let mismatched = runtime
        .read_workload_group_logs(&other_generation, &selection, &query(None))
        .await;
    let undeclared = runtime
        .read_workload_group_logs(
            &group_launch(),
            &RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
                name: "worker".to_owned(),
                label: "worker".to_owned(),
            }])
            .expect("selection"),
            &query(None),
        )
        .await;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let mut follow = runtime
        .follow_workload_group_logs(
            &group_launch(),
            &selection,
            dotcore_runtime::RuntimeGroupLogFollowQuery::new(
                &selection,
                BTreeMap::from([("web".to_owned(), at(10))]),
                cancellation.clone(),
            )
            .expect("follow query"),
        )
        .await
        .expect("follow")
        .pop()
        .expect("one source")
        .result
        .expect("an open source");
    let followed = [follow.next().await, follow.next().await].map(|event| match event {
        Some(Ok(RuntimeLogEvent::Entry(entry))) => entry.content,
        other => panic!("unexpected follow event {other:?}"),
    });
    cancellation.cancel();

    assert_eq!(before_create.len(), 1);
    assert_eq!(before_create[0].source, web);
    assert!(matches!(
        before_create[0].result,
        Err(RuntimeBackendError::WorkloadNotCreated)
    ));
    let newest = newest[0].result.as_ref().expect("newest page");
    assert_eq!(
        newest
            .entries
            .iter()
            .map(|entry| (entry.timestamp, entry.content.clone()))
            .collect::<Vec<_>>(),
        (10..=12)
            .map(|number| (at(number), line(number)))
            .collect::<Vec<_>>()
    );
    assert!(newest.has_older_output);
    assert_eq!(
        older[0]
            .result
            .as_ref()
            .expect("older page")
            .entries
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>(),
        (7..=9).map(line).collect::<Vec<_>>()
    );
    assert!(matches!(
        mismatched,
        Err(RuntimeBackendError::InvalidLaunch(_))
    ));
    assert!(matches!(
        undeclared,
        Err(RuntimeBackendError::InvalidLaunch(_))
    ));
    assert_eq!(followed, [line(11), line(12)]);
    assert!(
        follow.next().await.is_none(),
        "cancellation ends the follow"
    );
}

#[tokio::test]
async fn retains_only_the_latest_absence_confirmation() {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    {
        let mut state = runtime.state.lock().expect("simulated runtime state");
        state
            .workloads
            .insert("first".to_owned(), workload("first"));
        state
            .workloads
            .insert("second".to_owned(), workload("second"));
    }

    runtime
        .remove_workload(&WorkloadId("first".to_owned()))
        .await
        .expect("remove first workload");
    assert!(matches!(
        runtime
            .inspect_workload(&WorkloadId("first".to_owned()))
            .await,
        Ok(WorkloadObservation::Absent(_))
    ));

    runtime
        .remove_workload(&WorkloadId("second".to_owned()))
        .await
        .expect("remove second workload");
    assert!(
        runtime
            .inspect_workload(&WorkloadId("first".to_owned()))
            .await
            .is_err()
    );
    assert!(matches!(
        runtime
            .inspect_workload(&WorkloadId("second".to_owned()))
            .await,
        Ok(WorkloadObservation::Absent(_))
    ));
}

fn log_source() -> RuntimeLogSource {
    RuntimeLogSource {
        name: "model".to_owned(),
        label: "Model".to_owned(),
    }
}

fn log_timestamp(line: u64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + 2 * line)
}

/// The exclusive page boundary the management pipeline derives from the oldest returned line.
fn page_boundary_before(line: u64) -> std::time::SystemTime {
    log_timestamp(line) - std::time::Duration::from_secs(1)
}

fn log_line(line: u64) -> String {
    format!("simulated workload log line {line:03} owner=deployment-01 generation=1")
}

fn history_query(
    maximum_entries: u16,
    before_timestamp: Option<std::time::SystemTime>,
) -> RuntimeWorkloadLogQuery {
    RuntimeWorkloadLogQuery::new(
        log_source(),
        RuntimeLogQuery::new(
            std::num::NonZeroU16::new(maximum_entries).expect("non-zero entry limit"),
            before_timestamp,
        )
        .expect("bounded query"),
    )
}

fn follow_query(
    since: Option<std::time::SystemTime>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> RuntimeWorkloadLogFollowQuery {
    RuntimeWorkloadLogFollowQuery::new(
        log_source(),
        RuntimeLogFollowQuery::new(since, cancellation.clone()),
    )
}

async fn runtime_with_workload() -> InMemoryRuntimeBackend {
    let runtime = InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64());
    runtime
        .create_workload(&individual_workload_launch())
        .await
        .expect("create simulated workload");
    runtime
}

fn entry_lines(entries: &[RuntimeLogEntry]) -> Vec<(std::time::SystemTime, String)> {
    entries
        .iter()
        .map(|entry| {
            assert_eq!(entry.source, log_source());
            assert_eq!(entry.stream, RuntimeLogStreamKind::Stdout);
            assert!(!entry.truncated);
            (
                entry.timestamp,
                String::from_utf8(entry.content.clone()).expect("UTF-8 line"),
            )
        })
        .collect()
}

fn expected_lines(lines: std::ops::RangeInclusive<u64>) -> Vec<(std::time::SystemTime, String)> {
    lines
        .map(|line| (log_timestamp(line), log_line(line)))
        .collect()
}

#[tokio::test]
async fn simulated_history_pages_newest_first_with_older_output_flag() {
    let runtime = runtime_with_workload().await;
    let correlation = individual_workload_launch().correlation;

    let newest = runtime
        .read_workload_logs_by_correlation(&correlation, &history_query(5, None))
        .await
        .expect("newest page");
    assert_eq!(entry_lines(&newest.entries), expected_lines(8..=12));
    assert!(newest.has_older_output);
    assert!(!newest.candidate_budget_truncated);

    let middle = runtime
        .read_workload_logs_by_correlation(
            &correlation,
            &history_query(5, Some(page_boundary_before(8))),
        )
        .await
        .expect("middle page");
    assert_eq!(entry_lines(&middle.entries), expected_lines(3..=7));
    assert!(middle.has_older_output);

    let oldest = runtime
        .read_workload_logs_by_correlation(
            &correlation,
            &history_query(5, Some(page_boundary_before(3))),
        )
        .await
        .expect("oldest page");
    assert_eq!(entry_lines(&oldest.entries), expected_lines(1..=2));
    assert!(!oldest.has_older_output);

    let all = runtime
        .read_workload_logs_by_correlation(&correlation, &history_query(200, None))
        .await
        .expect("full history");
    assert_eq!(entry_lines(&all.entries), expected_lines(1..=12));
    assert!(!all.has_older_output);

    let exact = runtime
        .read_workload_logs_by_correlation(&correlation, &history_query(12, None))
        .await
        .expect("exact-fit page");
    assert_eq!(exact.entries.len(), 12);
    assert!(!exact.has_older_output);

    let before_everything = runtime
        .read_workload_logs_by_correlation(
            &correlation,
            &history_query(5, Some(page_boundary_before(1))),
        )
        .await
        .expect("page before the first line");
    assert!(before_everything.entries.is_empty());
    assert!(!before_everything.has_older_output);
}

#[tokio::test]
async fn simulated_follow_replays_lines_after_the_resume_position_then_stays_open() {
    let runtime = runtime_with_workload().await;
    let correlation = individual_workload_launch().correlation;
    let cancellation = tokio_util::sync::CancellationToken::new();

    let mut stream = runtime
        .follow_workload_logs_by_correlation(&correlation, follow_query(None, &cancellation))
        .await
        .expect("follow stream");
    let mut entries = Vec::new();
    for _ in 0..12 {
        match stream.next().await.expect("replayed line").expect("event") {
            RuntimeLogEvent::Entry(entry) => entries.push(entry),
            RuntimeLogEvent::SourceUnavailable(_) => panic!("source must stay available"),
        }
    }
    assert_eq!(entry_lines(&entries), expected_lines(1..=12));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
            .await
            .is_err(),
        "follow stays open after the replay"
    );
    cancellation.cancel();
    assert!(stream.next().await.is_none());

    // A resume position is exclusive: the entry already delivered is not repeated.
    let cancellation = tokio_util::sync::CancellationToken::new();
    let mut resumed = runtime
        .follow_workload_logs_by_correlation(
            &correlation,
            follow_query(Some(log_timestamp(10)), &cancellation),
        )
        .await
        .expect("resumed stream");
    let mut entries = Vec::new();
    for _ in 0..2 {
        match resumed.next().await.expect("resumed line").expect("event") {
            RuntimeLogEvent::Entry(entry) => entries.push(entry),
            RuntimeLogEvent::SourceUnavailable(_) => panic!("source must stay available"),
        }
    }
    assert_eq!(entry_lines(&entries), expected_lines(11..=12));
    cancellation.cancel();
    assert!(resumed.next().await.is_none());
}

#[tokio::test]
async fn simulated_logs_reject_an_unknown_correlation_as_not_created() {
    let runtime = runtime_with_workload().await;
    let launch = individual_workload_launch();
    let cancellation = tokio_util::sync::CancellationToken::new();

    let other_owner = WorkloadCorrelation {
        owner_id: "deployment-02".to_owned(),
        ..launch.correlation.clone()
    };
    let other_generation = WorkloadCorrelation {
        reconcile_generation: launch.correlation.reconcile_generation + 1,
        ..launch.correlation.clone()
    };
    for correlation in [&other_owner, &other_generation] {
        let read = runtime
            .read_workload_logs_by_correlation(correlation, &history_query(5, None))
            .await
            .expect_err("unknown correlation read");
        assert!(matches!(read, RuntimeBackendError::WorkloadNotCreated));
        let follow = match runtime
            .follow_workload_logs_by_correlation(correlation, follow_query(None, &cancellation))
            .await
        {
            Ok(_) => panic!("unknown correlation must not follow"),
            Err(error) => error,
        };
        assert!(matches!(follow, RuntimeBackendError::WorkloadNotCreated));
    }

    // Removal returns the correlation to "not created".
    runtime
        .remove_workload(&WorkloadId("simulated-workload-1".to_owned()))
        .await
        .expect("remove workload");
    let removed = runtime
        .read_workload_logs_by_correlation(&launch.correlation, &history_query(5, None))
        .await
        .expect_err("removed workload read");
    assert!(matches!(removed, RuntimeBackendError::WorkloadNotCreated));
}

#[tokio::test]
async fn simulated_follow_honors_a_cancelled_token() {
    let runtime = runtime_with_workload().await;
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let error = match runtime
        .follow_workload_logs_by_correlation(
            &individual_workload_launch().correlation,
            follow_query(None, &cancellation),
        )
        .await
    {
        Ok(_) => panic!("cancelled follow must not open"),
        Err(error) => error,
    };
    assert!(error.is_log_cancelled());
}
