//! Node-qualified host process identity for runtime and node observation.
//!
//! An opaque process start token distinguishes a current process from a later process that
//! reuses the same numeric PID, and node qualification prevents identities from crossing machines.
//! The raw per-process facts and ancestry live in the host process crate.

use crate::SiteNodeId;

/// Node-qualified process identity that remains stable across PID reuse.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostProcessIdentity {
    pub managed_node_id: SiteNodeId,
    pub process_id: u32,
    pub process_start_token: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn one_process_on_two_nodes_keeps_distinct_identities_and_keys() {
        let node_a = SiteNodeId::for_test("node-a");
        let node_b = SiteNodeId::for_test("node-b");
        let identity = |managed_node_id| HostProcessIdentity {
            managed_node_id,
            process_id: 4242,
            process_start_token: 9000,
        };

        let on_node_a = identity(node_a.clone());
        let on_node_b = identity(node_b.clone());

        assert_eq!(on_node_a, identity(node_a));
        assert_ne!(on_node_a, on_node_b);
        assert_eq!(
            BTreeSet::from([on_node_a.clone(), on_node_b.clone()]).len(),
            2
        );

        let mut processes = BTreeMap::new();
        processes.insert(on_node_a.clone(), "node-a-process");
        processes.insert(on_node_b.clone(), "node-b-process");
        assert_eq!(processes.get(&on_node_a), Some(&"node-a-process"));
        assert_eq!(processes.get(&on_node_b), Some(&"node-b-process"));
    }
}
