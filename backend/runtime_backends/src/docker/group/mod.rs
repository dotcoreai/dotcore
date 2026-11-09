//! Docker workload-group lifecycle responsibilities.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use dotcore_runtime::{
    RuntimeBackendError, RuntimeFuture, WorkloadGroupLaunch, WorkloadGroupObservation,
    WorkloadUnitName,
};

use super::{
    adapter::{DockerAdapter, container_host_process},
    client::{DockerClient, DockerNetwork, GroupContainerInspection},
    runtime_error, runtime_error_with_source,
};

mod creation;
mod discovery;
mod errors;
mod inspection;
mod lifecycle;
mod readiness;
mod recovery;
mod registry;
mod removal;
mod request;
mod resources;
mod types;
mod validation;
mod verification;

pub(super) use discovery::{
    SiteResetGroupIdentity, site_reset_group_container_identity, site_reset_group_network_identity,
};
pub use errors::{DockerGroupCreationError, DockerGroupStartError, OwnedWorkloadGroupRealization};
use removal::*;
use request::*;
use types::*;
pub(super) use types::{GROUP_SCHEMA, KnownGroup};
use validation::*;
use verification::*;
pub(super) use verification::{
    expected_group_labels, group_network_has_expected_ownership, validate_reset_unit_network,
};

#[cfg(test)]
mod tests;
