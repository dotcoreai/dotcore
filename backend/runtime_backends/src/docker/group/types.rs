//! Shared private types and ownership labels for Docker workload groups.

use std::collections::BTreeMap;

use dotcore_runtime::{ReadyLaunchGateOutcome, WorkloadGroupLaunch, WorkloadUnitName};

use super::super::request::DockerGroupContainerRequest;

pub const GROUP_SCHEMA: &str = "5";
pub(super) const LABEL_GROUP: &str = "com.dotcore.runtime.group";
pub(super) const LABEL_UNIT: &str = "com.dotcore.runtime.unit";
pub(super) const LABEL_NETWORK: &str = "com.dotcore.runtime.group-network";

#[derive(Clone)]
pub struct KnownGroup {
    pub(super) launch: WorkloadGroupLaunch,
    pub(super) network_id: String,
    pub(super) network_name: String,
    pub(super) units: BTreeMap<WorkloadUnitName, KnownGroupUnit>,
}

#[derive(Clone)]
pub(super) struct KnownGroupUnit {
    pub(super) container_id: String,
    pub(super) request: DockerGroupContainerRequest,
    pub(super) gate: Option<ReadyLaunchGateOutcome>,
}
