//! Apps-controller classification of runtime workload-group observations.

use dotcore_runtime::{
    ObservedWorkloadGroup, WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadUnitState,
};

use crate::dispatch::application::{ApplicationRuntimeMap, TargetWorkloadGroupObservation};

/// Ownership-safe runtime evidence classified by expected unit coverage and configuration freshness.
pub(super) enum ExactOwnedObservation<'a> {
    Complete {
        group: &'a ObservedWorkloadGroup,
        stale: bool,
    },
    Partial {
        group: &'a ObservedWorkloadGroup,
        stale: bool,
    },
    Absent,
}

pub(super) fn exact_owned_observation<'a>(
    candidate: &'a TargetWorkloadGroupObservation,
    launch: &WorkloadGroupLaunch,
    execution_target_id: &str,
    runtimes: &ApplicationRuntimeMap,
) -> Option<ExactOwnedObservation<'a>> {
    if candidate.execution_target_id != execution_target_id {
        return None;
    }
    let expected_backend = runtimes.backend_id(execution_target_id).ok()?;
    match &candidate.observation {
        WorkloadGroupObservation::Observed(group) => {
            exact_owned_group(group, launch, &expected_backend, false)
        }
        WorkloadGroupObservation::StaleConfiguration { group, .. } => {
            exact_owned_group(group, launch, &expected_backend, true)
        }
        WorkloadGroupObservation::Absent {
            group_id,
            correlation,
            backend_id,
        } => (*group_id == launch.group_id
            && *correlation == launch.correlation
            && *backend_id == expected_backend)
            .then_some(ExactOwnedObservation::Absent),
        WorkloadGroupObservation::Unknown { .. } => None,
    }
}

fn exact_owned_group<'a>(
    group: &'a ObservedWorkloadGroup,
    launch: &WorkloadGroupLaunch,
    expected_backend: &dotcore_runtime::RuntimeBackendId,
    stale: bool,
) -> Option<ExactOwnedObservation<'a>> {
    if group.group_id != launch.group_id
        || group.correlation != launch.correlation
        || group.backend_id != *expected_backend
        || !group
            .units
            .keys()
            .all(|name| launch.units.contains_key(name))
    {
        return None;
    }
    if group.units.keys().eq(launch.units.keys()) {
        Some(ExactOwnedObservation::Complete { group, stale })
    } else {
        Some(ExactOwnedObservation::Partial { group, stale })
    }
}

/// Returns an exact-owned retained group whose launch projection is older than the desired launch.
pub(super) fn stale_owned_group<'a>(
    candidate: &'a TargetWorkloadGroupObservation,
    launch: &WorkloadGroupLaunch,
    execution_target_id: &str,
    runtimes: &ApplicationRuntimeMap,
) -> Option<&'a ObservedWorkloadGroup> {
    if candidate.execution_target_id != execution_target_id {
        return None;
    }
    let expected_backend = runtimes.backend_id(execution_target_id).ok()?;
    let group = match &candidate.observation {
        WorkloadGroupObservation::Observed(group)
        | WorkloadGroupObservation::StaleConfiguration { group, .. } => group,
        WorkloadGroupObservation::Absent { .. } | WorkloadGroupObservation::Unknown { .. } => {
            return None;
        }
    };
    (group.group_id == launch.group_id
        && group.backend_id == expected_backend
        && group.correlation.owner_id == launch.correlation.owner_id
        && group.correlation != launch.correlation
        && group.units.keys().eq(launch.units.keys()))
    .then_some(group)
}

pub(super) fn observation_owner_id(candidate: &TargetWorkloadGroupObservation) -> Option<&str> {
    match &candidate.observation {
        WorkloadGroupObservation::Observed(group)
        | WorkloadGroupObservation::StaleConfiguration { group, .. } => {
            Some(&group.correlation.owner_id)
        }
        WorkloadGroupObservation::Absent { correlation, .. } => Some(&correlation.owner_id),
        WorkloadGroupObservation::Unknown { .. } => None,
    }
}

pub(super) fn observation_matches_group(
    candidate: &TargetWorkloadGroupObservation,
    launch: &WorkloadGroupLaunch,
) -> bool {
    match &candidate.observation {
        WorkloadGroupObservation::Observed(group)
        | WorkloadGroupObservation::StaleConfiguration { group, .. } => {
            group.group_id == launch.group_id
        }
        WorkloadGroupObservation::Absent { group_id, .. }
        | WorkloadGroupObservation::Unknown { group_id, .. } => *group_id == launch.group_id,
    }
}

pub(super) fn group_has_stopped_unit(group: &ObservedWorkloadGroup) -> bool {
    group
        .units
        .values()
        .any(|unit| unit.state == WorkloadUnitState::Stopped)
}

pub(super) fn group_has_absent_unit(group: &ObservedWorkloadGroup) -> bool {
    group
        .units
        .values()
        .any(|unit| unit.state == WorkloadUnitState::Absent)
}

pub(super) fn group_has_active_or_unavailable_unit(group: &ObservedWorkloadGroup) -> bool {
    group.units.values().any(|unit| {
        matches!(
            unit.state,
            WorkloadUnitState::Starting
                | WorkloadUnitState::Running
                | WorkloadUnitState::Unavailable
        )
    })
}

pub(super) fn group_has_stopping_unit(group: &ObservedWorkloadGroup) -> bool {
    group
        .units
        .values()
        .any(|unit| unit.state == WorkloadUnitState::Stopping)
}

pub(super) fn group_is_all_stopped(group: &ObservedWorkloadGroup) -> bool {
    group
        .units
        .values()
        .all(|unit| unit.state == WorkloadUnitState::Stopped)
}
