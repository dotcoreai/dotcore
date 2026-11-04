//! Hardware observations keyed by managed node, as a hosting node reads them from its own
//! reporter's collections to attribute accelerator memory.
//!
//! A reader takes one collection at a time and keeps it, so everything it attributes from that
//! collection agrees even while newer ones are published. Readers address nodes by validated
//! identity and never trigger collection or fall back to another node. A failed collection reads
//! as a failed refresh, never as an older fact.

use std::collections::BTreeMap;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, RwLock};

use dotcore_domain::SiteNodeId;
use dotcore_domain::hardware_observation::{
    AcceleratorProcessObservation, NodeHardwareObservation,
};
use dotcore_node_hardware::hardware::timestamp_from;
use dotcore_proto::{
    dotcore::observation::v1::{HardwareSnapshot, ObservationUnavailable},
    dotcore::site::private::v1::node_hardware_observation,
    dotcore::site::v1::{HardwareRefreshState, hardware_refresh_state},
    google::protobuf::{Empty, Timestamp},
};

use crate::node::agent::{CollectedNodeObservation, NodeObservationFeed};

#[derive(Clone, Debug, PartialEq)]
pub struct HardwareInventoryEntry {
    pub snapshot: Option<HardwareSnapshot>,
    pub refresh: HardwareRefreshState,
    pub accelerator_processes: Option<AcceleratorProcessObservation>,
}

impl HardwareInventoryEntry {
    pub(crate) fn pending() -> Self {
        Self {
            snapshot: None,
            refresh: HardwareRefreshState {
                attempted_at: None,
                outcome: Some(hardware_refresh_state::Outcome::Pending(Empty {})),
            },
            accelerator_processes: None,
        }
    }

    pub fn succeeded(observation: NodeHardwareObservation, attempted_at: Timestamp) -> Self {
        Self {
            snapshot: Some(observation.snapshot),
            refresh: HardwareRefreshState {
                attempted_at: Some(attempted_at),
                outcome: Some(hardware_refresh_state::Outcome::Succeeded(Empty {})),
            },
            accelerator_processes: Some(observation.accelerator_processes),
        }
    }

    /// An entry whose only collection attempt failed, so no snapshot or process rows exist.
    fn failed(attempted_at: Timestamp, unavailable: ObservationUnavailable) -> Self {
        Self {
            snapshot: None,
            refresh: HardwareRefreshState {
                attempted_at: Some(attempted_at),
                outcome: Some(hardware_refresh_state::Outcome::Failed(unavailable)),
            },
            accelerator_processes: None,
        }
    }
}

/// Source of a hosting node's hardware collections for accelerator memory attribution.
pub trait HardwareInventorySource: Send + Sync {
    /// Returns the current collection. The returned value never changes, so every attribution
    /// made from it reads the same collection.
    fn collection(&self) -> HardwareCollection;

    /// Revision of the first collection that begins reading hardware after this call: a process
    /// that exists now may be absent from every collection before it, so only from this revision
    /// on does its absence mean it holds no accelerator memory.
    fn next_revision(&self) -> u64;
}

/// One hardware collection as attribution reads it: the entry of each managed node it answers
/// for and the revision of the collection those entries came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HardwareCollection {
    revision: u64,
    entries: BTreeMap<SiteNodeId, HardwareInventoryEntry>,
}

impl HardwareCollection {
    /// Revision of the node observation this collection was published with; 0 before the first
    /// collection completes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The entry of `managed_node_id`, or none when this collection does not answer for it.
    pub fn entry(&self, managed_node_id: &SiteNodeId) -> Option<&HardwareInventoryEntry> {
        self.entries.get(managed_node_id)
    }
}

/// In-memory publisher of per-node hardware entries that stands in for a node's observations in
/// tests of inventory consumers. Each publication is a new collection revision.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct HardwareInventoryPublisher {
    current: Arc<RwLock<HardwareCollection>>,
}

/// Reader side of [`HardwareInventoryPublisher`].
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct HardwareInventory {
    current: Arc<RwLock<HardwareCollection>>,
}

#[cfg(any(test, feature = "test-support"))]
impl HardwareInventoryPublisher {
    pub fn new() -> (Self, HardwareInventory) {
        let current = Arc::new(RwLock::new(HardwareCollection::default()));
        (
            Self {
                current: Arc::clone(&current),
            },
            HardwareInventory { current },
        )
    }

    #[cfg(test)]
    pub(crate) fn register(&self, managed_node_id: &SiteNodeId) {
        self.write()
            .entries
            .entry(managed_node_id.clone())
            .or_insert_with(HardwareInventoryEntry::pending);
    }

    /// Publishes a new collection revision in which `managed_node_id` reads `entry`.
    pub fn publish(&self, managed_node_id: &SiteNodeId, entry: HardwareInventoryEntry) {
        let mut current = self.write();
        current.revision += 1;
        current.entries.insert(managed_node_id.clone(), entry);
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HardwareCollection> {
        self.current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl HardwareInventorySource for HardwareInventory {
    fn collection(&self) -> HardwareCollection {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn next_revision(&self) -> u64 {
        self.collection().revision + 1
    }
}

/// Hosting-node inventory read from its own reporter's latest collection, so the node attributes
/// accelerator memory without a second hardware collector. It answers only for its own node: a
/// failed collection reads as a failed refresh without a snapshot, never as an older fact.
#[derive(Clone)]
pub struct NodeObservationInventory {
    node_id: SiteNodeId,
    observations: NodeObservationFeed,
}

impl NodeObservationInventory {
    pub fn new(node_id: SiteNodeId, observations: NodeObservationFeed) -> Self {
        Self {
            node_id,
            observations,
        }
    }
}

impl HardwareInventorySource for NodeObservationInventory {
    fn collection(&self) -> HardwareCollection {
        let (revision, entry) = match self.observations.latest() {
            Some(collected) => (collected.revision, collected_entry(collected)),
            None => (0, HardwareInventoryEntry::pending()),
        };
        HardwareCollection {
            revision,
            entries: BTreeMap::from([(self.node_id.clone(), entry)]),
        }
    }

    fn next_revision(&self) -> u64 {
        self.observations.next_revision()
    }
}

/// The inventory entry of one completed collection.
fn collected_entry(collected: CollectedNodeObservation) -> HardwareInventoryEntry {
    let attempted_at = timestamp_from(collected.collected_at);
    let hardware = collected
        .snapshot
        .hardware
        .and_then(|hardware| hardware.result);
    match (hardware, collected.accelerator_processes) {
        (
            Some(node_hardware_observation::Result::Snapshot(snapshot)),
            Some(accelerator_processes),
        ) => HardwareInventoryEntry::succeeded(
            NodeHardwareObservation {
                snapshot,
                accelerator_processes,
            },
            attempted_at,
        ),
        (Some(node_hardware_observation::Result::Unavailable(unavailable)), _) => {
            HardwareInventoryEntry::failed(attempted_at, unavailable)
        }
        _ => HardwareInventoryEntry::failed(
            attempted_at,
            ObservationUnavailable {
                code: "COLLECTION_INCOMPLETE".to_owned(),
                message: "The node hardware collection is incomplete.".to_owned(),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::SiteNodeId;
    use dotcore_proto::dotcore::observation::v1::{HostFacts, HostObservation, host_observation};

    use super::*;

    #[test]
    fn pending_is_node_keyed_without_singleton_fallback() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publisher.register(&SiteNodeId::for_test("node-a"));

        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-a"))
                .cloned(),
            Some(HardwareInventoryEntry::pending())
        );
        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-b"))
                .cloned(),
            None
        );
    }

    #[test]
    fn publication_replaces_only_the_addressed_node() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let first_a = succeeded("first-a", 1);
        let first_b = succeeded("first-b", 2);
        let second_a = succeeded("second-a", 3);
        publisher.publish(&SiteNodeId::for_test("node-a"), first_a);
        publisher.publish(&SiteNodeId::for_test("node-b"), first_b.clone());

        publisher.publish(&SiteNodeId::for_test("node-a"), second_a.clone());

        assert_eq!(inventory.collection().revision(), 3);
        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-a"))
                .cloned(),
            Some(second_a)
        );
        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-b"))
                .cloned(),
            Some(first_b)
        );
        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-c"))
                .cloned(),
            None
        );
    }

    #[test]
    fn registration_never_discards_an_existing_observation() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let observation = succeeded("complete", 4);
        publisher.publish(&SiteNodeId::for_test("node-a"), observation.clone());

        publisher.register(&SiteNodeId::for_test("node-a"));

        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-a"))
                .cloned(),
            Some(observation)
        );
    }

    #[test]
    fn node_inventory_reads_its_reporter_collections_only_for_its_own_node() {
        let feed = NodeObservationFeed::default();
        let inventory = NodeObservationInventory::new(SiteNodeId::for_test("node-a"), feed.clone());
        assert_eq!(inventory.collection().revision(), 0);
        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-a"))
                .cloned(),
            Some(HardwareInventoryEntry::pending())
        );

        let collected_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(20);
        let expected = succeeded("collected", 20);
        feed.publish(
            crate::node::agent::NodeCollection {
                snapshot: dotcore_proto::dotcore::site::private::v1::NodeObservationSnapshot {
                    hardware: Some(
                        dotcore_proto::dotcore::site::private::v1::NodeHardwareObservation {
                            result: Some(node_hardware_observation::Result::Snapshot(
                                expected.snapshot.clone().expect("snapshot"),
                            )),
                        },
                    ),
                    runtimes: Vec::new(),
                },
                accelerator_processes: expected.accelerator_processes.clone(),
            },
            7,
            collected_at,
        );
        let pinned = inventory.collection();
        assert_eq!(pinned.revision(), 7);
        assert_eq!(
            pinned.entry(&SiteNodeId::for_test("node-a")),
            Some(&expected)
        );
        assert_eq!(pinned.entry(&SiteNodeId::for_test("node-b")), None);

        let unavailable = ObservationUnavailable {
            code: "COLLECTION_TIMED_OUT".to_owned(),
            message: "Hardware observation timed out on this worker.".to_owned(),
        };
        feed.publish(
            crate::node::agent::NodeCollection {
                snapshot: dotcore_proto::dotcore::site::private::v1::NodeObservationSnapshot {
                    hardware: Some(
                        dotcore_proto::dotcore::site::private::v1::NodeHardwareObservation {
                            result: Some(node_hardware_observation::Result::Unavailable(
                                unavailable.clone(),
                            )),
                        },
                    ),
                    runtimes: Vec::new(),
                },
                accelerator_processes: None,
            },
            8,
            collected_at,
        );
        assert_eq!(
            pinned.entry(&SiteNodeId::for_test("node-a")),
            Some(&expected),
            "a pinned collection never moves to a newer publication"
        );
        // A failed collection never re-reports an earlier snapshot.
        assert_eq!(inventory.collection().revision(), 8);
        assert_eq!(
            inventory
                .collection()
                .entry(&SiteNodeId::for_test("node-a"))
                .cloned(),
            Some(HardwareInventoryEntry {
                snapshot: None,
                refresh: HardwareRefreshState {
                    attempted_at: Some(timestamp(20)),
                    outcome: Some(hardware_refresh_state::Outcome::Failed(unavailable)),
                },
                accelerator_processes: None,
            })
        );
    }

    fn succeeded(host_name: &str, seconds: i64) -> HardwareInventoryEntry {
        HardwareInventoryEntry::succeeded(
            NodeHardwareObservation {
                snapshot: HardwareSnapshot {
                    observed_at: Some(timestamp(seconds)),
                    host: Some(HostObservation {
                        result: Some(host_observation::Result::Facts(HostFacts {
                            host_name: host_name.to_owned(),
                            ..Default::default()
                        })),
                    }),
                    accelerators: Vec::new(),
                },
                accelerator_processes: AcceleratorProcessObservation::Observed(Vec::new()),
            },
            timestamp(seconds),
        )
    }

    fn timestamp(seconds: i64) -> Timestamp {
        Timestamp { seconds, nanos: 0 }
    }
}
