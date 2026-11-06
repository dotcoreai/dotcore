//! Host CPU and memory attribution for the application workload groups a node runs, by unit.
//!
//! The node reads its runtime's figures for every group it realizes with each hardware
//! collection, so a status attributes them from the same collection as everything else. CPU use
//! is the share of the hosting machine's logical CPUs a unit kept busy between two consecutive
//! readings; a unit's first reading, or one after its CPU time started again, has no CPU share. A
//! unit the runtime gives no figures reads as having none, never as a false zero.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use dotcore_domain::SiteNodeId;
use dotcore_proto::dotcore::observation::v1::host_observation;
use dotcore_runtime::{
    WorkloadGroupLaunch, WorkloadUnitName, WorkloadUnitResourceObservation, WorkloadUnitResources,
};
use dotcore_site::node::{
    agent::CollectionReading,
    hardware_inventory::{HardwareCollection, HardwareInventorySource},
};
use tracing::debug;

use crate::dispatch::application::ApplicationRuntimeMap;

/// Host resources one application service used, as attributed from one hardware collection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ServiceHostResources {
    /// When the runtime measured the service.
    pub(crate) observed_at: SystemTime,

    /// Host memory the service's processes hold, not counting reclaimable file cache.
    pub(crate) memory_bytes: u64,

    /// Share, from 0 to 1, of the hosting machine's logical CPUs the service kept busy since the
    /// previous reading; none on its first reading or when the machine's CPU count is unknown.
    pub(crate) cpu_utilization: Option<f64>,
}

/// Attributes host CPU and memory to the units of the application groups this node realizes.
/// Clones share the same groups and readings, so the realizer that tracks groups, the collection
/// that reads them and the status that projects them use one view.
#[derive(Clone)]
pub(super) struct GroupResourceAttribution {
    inventory: Arc<dyn HardwareInventorySource>,
    port: Arc<dyn GroupResourcesPort>,
    state: Arc<Mutex<ResourceState>>,
}

impl GroupResourceAttribution {
    pub(super) fn new(
        inventory: Arc<dyn HardwareInventorySource>,
        port: Arc<dyn GroupResourcesPort>,
    ) -> Self {
        Self {
            inventory,
            port,
            state: Arc::default(),
        }
    }

    /// Reads the deployment's group, as `launch` on `execution_target_id`, with every collection
    /// from now on. A different launch than the one already read starts its units afresh.
    pub(super) fn track(
        &self,
        deployment_id: &str,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
    ) {
        let tracked = TrackedGroup {
            execution_target_id: execution_target_id.to_owned(),
            launch: launch.clone(),
        };
        let mut state = self.state();
        if state.tracked.get(deployment_id) == Some(&tracked) {
            return;
        }
        state.forget(deployment_id);
        state.tracked.insert(deployment_id.to_owned(), tracked);
    }

    /// Stops reading the deployment's group and drops every reading of it.
    pub(super) fn forget(&self, deployment_id: &str) {
        self.state().forget(deployment_id);
    }

    /// Keeps reading exactly the deployments still assigned this pass.
    pub(super) fn retain(&self, retained: &BTreeSet<&str>) {
        let mut state = self.state();
        let dropped = state
            .tracked
            .keys()
            .filter(|deployment_id| !retained.contains(deployment_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for deployment_id in dropped {
            state.forget(&deployment_id);
        }
    }

    /// The node's current hardware collection, for a reader that attributes outside a status.
    pub(super) fn collection(&self) -> HardwareCollection {
        self.inventory.collection()
    }

    /// The host resources of every unit of the deployment read with `collection`, by unit name,
    /// with CPU shares of the logical CPUs `collection` reports for `node`. A unit without
    /// figures is not listed.
    pub(super) fn projection(
        &self,
        deployment_id: &str,
        node: &SiteNodeId,
        collection: &HardwareCollection,
    ) -> BTreeMap<String, ServiceHostResources> {
        let logical_cpus = logical_cpus(collection, node);
        let state = self.state();
        let Some((_, readings)) = state
            .readings
            .iter()
            .find(|(revision, _)| *revision == collection.revision())
        else {
            return BTreeMap::new();
        };
        readings
            .get(deployment_id)
            .map(|units| {
                units
                    .iter()
                    .map(|(unit, reading)| (unit.clone(), reading.resources(logical_cpus)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Reads every tracked group for the collection reported as `revision`. A reading of a group
    /// whose tracking ended or changed meanwhile is dropped.
    async fn read(&self, revision: u64) {
        let tracked = self
            .state()
            .tracked
            .iter()
            .map(|(deployment_id, tracked)| (deployment_id.clone(), tracked.clone()))
            .collect::<Vec<_>>();
        let observed = futures_util::future::join_all(tracked.into_iter().map(
            |(deployment_id, tracked)| async move {
                let units = self
                    .port
                    .observe(&tracked.execution_target_id, &tracked.launch)
                    .await;
                (deployment_id, tracked, units)
            },
        ))
        .await;

        let mut state = self.state();
        let mut readings = BTreeMap::new();
        for (deployment_id, tracked, units) in observed {
            if state.tracked.get(&deployment_id) != Some(&tracked) {
                continue;
            }
            let units = state.record_units(&deployment_id, units);
            if !units.is_empty() {
                readings.insert(deployment_id, units);
            }
        }
        state.readings.push_back((revision, readings));
        while state.readings.len() > READ_COLLECTIONS_KEPT {
            state.readings.pop_front();
        }
    }

    fn state(&self) -> MutexGuard<'_, ResourceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CollectionReading for GroupResourceAttribution {
    fn read(&self, revision: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(GroupResourceAttribution::read(self, revision))
    }
}

/// Read-only access to the CPU time and memory a runtime reports for the units of its groups. It
/// carries no lifecycle powers, so attribution can never act on a runtime.
pub(super) trait GroupResourcesPort: Send + Sync {
    /// Reads each unit of `launch` on `execution_target_id`; a group the runtime could not read
    /// lists no unit.
    fn observe<'a>(
        &'a self,
        execution_target_id: &'a str,
        launch: &'a WorkloadGroupLaunch,
    ) -> Pin<
        Box<
            dyn Future<Output = BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>>
                + Send
                + 'a,
        >,
    >;
}

impl GroupResourcesPort for ApplicationRuntimeMap {
    fn observe<'a>(
        &'a self,
        execution_target_id: &'a str,
        launch: &'a WorkloadGroupLaunch,
    ) -> Pin<
        Box<
            dyn Future<Output = BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.observe_group_resources(execution_target_id, launch)
                .await
                .unwrap_or_else(|error| {
                    debug!(
                        event_kind = "application_group_resources_observation_failed",
                        execution_target_id = %execution_target_id,
                        deployment_id = %launch.correlation.owner_id,
                        error = %error,
                        "Application group resources could not be observed"
                    );
                    BTreeMap::new()
                })
        })
    }
}

/// Collections whose readings are kept: the latest published one, which a status may have
/// pinned, the one being collected, and one more for a status still building.
const READ_COLLECTIONS_KEPT: usize = 3;

/// Units read per deployment and unit name.
type DeploymentReadings = BTreeMap<String, BTreeMap<String, UnitReading>>;

/// The groups read with each collection, each unit's latest cumulative CPU time, and the readings
/// taken with each recent collection.
#[derive(Default)]
struct ResourceState {
    tracked: BTreeMap<String, TrackedGroup>,

    /// Latest CPU time read of each unit, by deployment and unit name.
    previous: BTreeMap<String, BTreeMap<String, CpuTimeReading>>,

    /// Readings per collection revision, oldest first.
    readings: VecDeque<(u64, DeploymentReadings)>,
}

impl ResourceState {
    /// Keeps the figures of each unit of one deployment and derives its CPU use from the unit's
    /// previous reading. A unit without figures forgets its previous reading, so its next one
    /// starts afresh.
    fn record_units(
        &mut self,
        deployment_id: &str,
        units: BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>,
    ) -> BTreeMap<String, UnitReading> {
        let previous = self.previous.entry(deployment_id.to_owned()).or_default();
        let mut readings = BTreeMap::new();
        for (unit, observation) in units {
            let WorkloadUnitResources::Available {
                cpu_time_ns,
                memory_bytes,
            } = observation.resources
            else {
                previous.remove(&unit.0);
                continue;
            };
            let current = CpuTimeReading {
                cpu_time_ns,
                observed_at: observation.observed_at,
            };
            let cpu = previous
                .insert(unit.0.clone(), current)
                .and_then(|previous| current.used_since(&previous));
            readings.insert(
                unit.0,
                UnitReading {
                    observed_at: observation.observed_at,
                    memory_bytes,
                    cpu,
                },
            );
        }
        if previous.is_empty() {
            self.previous.remove(deployment_id);
        }
        readings
    }

    fn forget(&mut self, deployment_id: &str) {
        self.tracked.remove(deployment_id);
        self.previous.remove(deployment_id);
        for (_, readings) in &mut self.readings {
            readings.remove(deployment_id);
        }
    }
}

/// A group read with each collection: the target it runs on and its exact launch.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedGroup {
    execution_target_id: String,
    launch: WorkloadGroupLaunch,
}

/// A unit's cumulative CPU time at one instant.
#[derive(Clone, Copy, Debug)]
struct CpuTimeReading {
    cpu_time_ns: u64,
    observed_at: SystemTime,
}

impl CpuTimeReading {
    /// The CPU time used and the wall time elapsed since `previous`. A smaller CPU time than
    /// before means the unit restarted, and a reading not after the previous one measures no
    /// interval, so neither yields a use.
    fn used_since(&self, previous: &Self) -> Option<CpuUse> {
        let cpu_time_ns = self.cpu_time_ns.checked_sub(previous.cpu_time_ns)?;
        let wall = self.observed_at.duration_since(previous.observed_at).ok()?;
        let wall_ns = u64::try_from(wall.as_nanos())
            .ok()
            .filter(|wall| *wall > 0)?;
        Some(CpuUse {
            cpu_time_ns,
            wall_ns,
        })
    }
}

/// CPU time a unit used over an elapsed wall time.
#[derive(Clone, Copy, Debug)]
struct CpuUse {
    cpu_time_ns: u64,
    wall_ns: u64,
}

/// One unit's figures read with one collection.
#[derive(Clone, Copy, Debug)]
struct UnitReading {
    observed_at: SystemTime,
    memory_bytes: u64,
    cpu: Option<CpuUse>,
}

impl UnitReading {
    /// The unit's resources, its CPU use as a share of `logical_cpus`.
    fn resources(&self, logical_cpus: Option<u32>) -> ServiceHostResources {
        let cpu_utilization = self.cpu.zip(logical_cpus).map(|(cpu, logical_cpus)| {
            cpu.cpu_time_ns as f64 / (cpu.wall_ns as f64 * f64::from(logical_cpus))
        });
        ServiceHostResources {
            observed_at: self.observed_at,
            memory_bytes: self.memory_bytes,
            cpu_utilization,
        }
    }
}

/// The logical CPU count `collection` reports for `node`; none when it reports no host facts.
fn logical_cpus(collection: &HardwareCollection, node: &SiteNodeId) -> Option<u32> {
    let host = collection.entry(node)?.snapshot.as_ref()?.host.as_ref()?;
    match host.result.as_ref()? {
        host_observation::Result::Facts(facts) => Some(facts.cpu_count).filter(|count| *count > 0),
        host_observation::Result::Unavailable(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        num::NonZeroU16,
        time::{Duration, UNIX_EPOCH},
    };

    use dotcore_domain::{
        ExecutionPlatform,
        hardware_observation::{AcceleratorProcessObservation, NodeHardwareObservation},
    };
    use dotcore_proto::dotcore::observation::v1::{HardwareSnapshot, HostFacts, HostObservation};
    use dotcore_runtime::{
        ExecutionTarget, WorkloadCorrelation, WorkloadGroupId, WorkloadPublicationRequest,
        WorkloadUnitResourcesUnavailable,
    };
    use dotcore_site::node::hardware_inventory::{
        HardwareInventoryEntry, HardwareInventoryPublisher,
    };

    use super::*;

    #[tokio::test]
    async fn cpu_share_comes_from_two_readings_against_the_nodes_logical_cpus() {
        let fixture = Fixture::new(6);
        fixture.attribution.track("app-1", "target-a", &launch(1));

        // The first reading gives memory but no CPU share.
        fixture
            .read(&[("web", available(0, 4_000_000_000, 100))])
            .await;
        assert_eq!(
            fixture.projection("app-1"),
            BTreeMap::from([("web".to_owned(), resources(0, 100, None))])
        );

        // 3 CPU seconds over 2 wall seconds on 6 logical CPUs is a quarter of the machine.
        fixture
            .read(&[("web", available(2_000, 7_000_000_000, 200))])
            .await;
        assert_eq!(
            fixture.projection("app-1"),
            BTreeMap::from([("web".to_owned(), resources(2_000, 200, Some(0.25)))])
        );
    }

    #[tokio::test]
    async fn the_share_divides_by_the_logical_cpus_of_the_pinned_collection() {
        let fixture = Fixture::new(2);
        fixture.attribution.track("app-1", "target-a", &launch(1));
        fixture.read(&[("web", available(0, 0, 1))]).await;

        fixture
            .read(&[("web", available(1_000, 500_000_000, 1))])
            .await;

        assert_eq!(
            fixture.projection("app-1")["web"].cpu_utilization,
            Some(0.25)
        );
        fixture.report_logical_cpus(None);
        fixture
            .read(&[("web", available(2_000, 1_000_000_000, 1))])
            .await;
        assert_eq!(
            fixture.projection("app-1")["web"].cpu_utilization,
            None,
            "a collection without host facts gives no share"
        );
    }

    #[tokio::test]
    async fn a_smaller_cpu_time_is_a_restart_and_gives_no_share() {
        let fixture = Fixture::new(4);
        fixture.attribution.track("app-1", "target-a", &launch(1));
        fixture
            .read(&[("web", available(0, 9_000_000_000, 1))])
            .await;

        fixture
            .read(&[("web", available(1_000, 1_000_000_000, 1))])
            .await;
        assert_eq!(fixture.projection("app-1")["web"].cpu_utilization, None);

        // The restarted unit's next reading measures from its new start.
        fixture
            .read(&[("web", available(2_000, 2_000_000_000, 1))])
            .await;
        assert_eq!(
            fixture.projection("app-1")["web"].cpu_utilization,
            Some(0.25)
        );
    }

    #[tokio::test]
    async fn a_unit_without_figures_is_not_listed_and_starts_afresh() {
        let fixture = Fixture::new(4);
        fixture.attribution.track("app-1", "target-a", &launch(1));
        fixture
            .read(&[("cache", available(0, 0, 10)), ("web", available(0, 0, 20))])
            .await;

        fixture
            .read(&[("cache", unavailable(1_000)), ("web", not_reported(1_000))])
            .await;
        assert_eq!(fixture.projection("app-1"), BTreeMap::new());

        fixture
            .read(&[("cache", available(2_000, 4_000_000_000, 10))])
            .await;
        assert_eq!(
            fixture.projection("app-1"),
            BTreeMap::from([("cache".to_owned(), resources(2_000, 10, None))])
        );
    }

    #[tokio::test]
    async fn a_new_launch_or_a_forgotten_deployment_drops_earlier_readings() {
        let fixture = Fixture::new(4);
        fixture.attribution.track("app-1", "target-a", &launch(1));
        fixture.read(&[("web", available(0, 0, 1))]).await;

        fixture.attribution.track("app-1", "target-a", &launch(2));
        fixture
            .read(&[("web", available(1_000, 1_000_000_000, 2))])
            .await;
        assert_eq!(
            fixture.projection("app-1"),
            BTreeMap::from([("web".to_owned(), resources(1_000, 2, None))]),
            "the new launch's first reading has no share"
        );

        fixture.attribution.retain(&BTreeSet::new());
        assert_eq!(fixture.projection("app-1"), BTreeMap::new());
        fixture
            .read(&[("web", available(2_000, 2_000_000_000, 3))])
            .await;
        assert_eq!(fixture.projection("app-1"), BTreeMap::new());
        assert_eq!(fixture.port.reads(), 2, "an untracked group is not read");
    }

    /// Wall time the fixture's readings count from.
    const EPOCH_SECONDS: u64 = 1_700_000_000;

    /// An attribution over a fake runtime port and an inventory the test publishes a collection
    /// to before each reading.
    struct Fixture {
        attribution: GroupResourceAttribution,
        port: Arc<FakePort>,
        publisher: HardwareInventoryPublisher,

        /// Logical CPUs the host of each next collection reports; none reports no host facts.
        logical_cpus: Mutex<Option<u32>>,
    }

    impl Fixture {
        fn new(logical_cpus: u32) -> Self {
            let (publisher, inventory) = HardwareInventoryPublisher::new();
            let port = Arc::new(FakePort::default());
            Self {
                attribution: GroupResourceAttribution::new(
                    Arc::new(inventory),
                    Arc::clone(&port) as Arc<dyn GroupResourcesPort>,
                ),
                port,
                publisher,
                logical_cpus: Mutex::new(Some(logical_cpus)),
            }
        }

        /// Makes the host of each next collection report `logical_cpus`, or no host facts.
        fn report_logical_cpus(&self, logical_cpus: Option<u32>) {
            *self.logical_cpus.lock().expect("logical CPUs") = logical_cpus;
        }

        /// Publishes the next collection and takes its reading with the port reporting `units`.
        async fn read(&self, units: &[(&str, WorkloadUnitResourceObservation)]) {
            let host = self
                .logical_cpus
                .lock()
                .expect("logical CPUs")
                .map(|cpu_count| HostObservation {
                    result: Some(host_observation::Result::Facts(HostFacts {
                        cpu_count,
                        ..Default::default()
                    })),
                });
            self.publisher.publish(
                &node(),
                HardwareInventoryEntry::succeeded(
                    NodeHardwareObservation {
                        snapshot: HardwareSnapshot {
                            host,
                            ..Default::default()
                        },
                        accelerator_processes: AcceleratorProcessObservation::Observed(Vec::new()),
                    },
                    Default::default(),
                ),
            );
            self.port.report(units);
            let revision = self.attribution.collection().revision();
            GroupResourceAttribution::read(&self.attribution, revision).await;
        }

        fn projection(&self, deployment_id: &str) -> BTreeMap<String, ServiceHostResources> {
            self.attribution
                .projection(deployment_id, &node(), &self.attribution.collection())
        }
    }

    fn node() -> SiteNodeId {
        SiteNodeId::for_test("node-a")
    }

    fn at(millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS) + Duration::from_millis(millis)
    }

    fn available(
        millis: u64,
        cpu_time_ns: u64,
        memory_bytes: u64,
    ) -> WorkloadUnitResourceObservation {
        WorkloadUnitResourceObservation {
            observed_at: at(millis),
            resources: WorkloadUnitResources::Available {
                cpu_time_ns,
                memory_bytes,
            },
        }
    }

    fn unavailable(millis: u64) -> WorkloadUnitResourceObservation {
        WorkloadUnitResourceObservation {
            observed_at: at(millis),
            resources: WorkloadUnitResources::Unavailable(
                WorkloadUnitResourcesUnavailable::NotRunning,
            ),
        }
    }

    fn not_reported(millis: u64) -> WorkloadUnitResourceObservation {
        WorkloadUnitResourceObservation {
            observed_at: at(millis),
            resources: WorkloadUnitResources::NotReported,
        }
    }

    fn resources(
        millis: u64,
        memory_bytes: u64,
        cpu_utilization: Option<f64>,
    ) -> ServiceHostResources {
        ServiceHostResources {
            observed_at: at(millis),
            memory_bytes,
            cpu_utilization,
        }
    }

    /// The group launch of `app-1` at reconcile generation `generation`.
    fn launch(generation: u64) -> WorkloadGroupLaunch {
        WorkloadGroupLaunch {
            group_id: WorkloadGroupId("app-1".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "app-1".to_owned(),
                reconcile_generation: generation,
            },
            execution_target: ExecutionTarget {
                managed_node_id: node(),
                accelerator_device_ids: Vec::new(),
            },
            execution_target_id: "target-a".to_owned(),
            required_platform: ExecutionPlatform::linux_amd64(),
            units: BTreeMap::new(),
            publication: WorkloadPublicationRequest {
                unit_name: WorkloadUnitName("web".to_owned()),
                port_name: "http".to_owned(),
                configured_host: Ipv4Addr::LOCALHOST,
                host_port: NonZeroU16::new(30_000).expect("nonzero port"),
            },
            host_aliases: Default::default(),
        }
    }

    /// Runtime port reporting the unit figures the test sets for the next read, counting reads.
    #[derive(Default)]
    struct FakePort {
        units: Mutex<Vec<(String, WorkloadUnitResourceObservation)>>,
        reads: Mutex<usize>,
    }

    impl FakePort {
        fn report(&self, units: &[(&str, WorkloadUnitResourceObservation)]) {
            *self.units.lock().expect("reported units") = units
                .iter()
                .map(|(unit, observation)| ((*unit).to_owned(), copy(observation)))
                .collect();
        }

        fn reads(&self) -> usize {
            *self.reads.lock().expect("read count")
        }
    }

    impl GroupResourcesPort for FakePort {
        fn observe<'a>(
            &'a self,
            _: &'a str,
            _: &'a WorkloadGroupLaunch,
        ) -> Pin<
            Box<
                dyn Future<Output = BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>>
                    + Send
                    + 'a,
            >,
        > {
            *self.reads.lock().expect("read count") += 1;
            let units = self
                .units
                .lock()
                .expect("reported units")
                .iter()
                .map(|(unit, observation)| (WorkloadUnitName(unit.clone()), copy(observation)))
                .collect();
            Box::pin(std::future::ready(units))
        }
    }

    /// A copy of a runtime observation, which is not `Clone` because a read error carries its
    /// source; the fake reports only figures and not-running units.
    fn copy(observation: &WorkloadUnitResourceObservation) -> WorkloadUnitResourceObservation {
        let resources = match &observation.resources {
            WorkloadUnitResources::Available {
                cpu_time_ns,
                memory_bytes,
            } => WorkloadUnitResources::Available {
                cpu_time_ns: *cpu_time_ns,
                memory_bytes: *memory_bytes,
            },
            WorkloadUnitResources::Unavailable(_) => {
                WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::NotRunning)
            }
            WorkloadUnitResources::NotReported => WorkloadUnitResources::NotReported,
        };
        WorkloadUnitResourceObservation {
            observed_at: observation.observed_at,
            resources,
        }
    }
}
