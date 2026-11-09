//! Resource use of Docker workload-group units: CPU time and memory, read from the Docker Engine.
//!
//! Each unit's figures come from one Engine stats reading of its container, which works the same
//! on a Linux host and in Docker Desktop's VM. Memory is the working set the Docker CLI shows: the
//! container's charged memory without the file cache the kernel can reclaim. A unit whose
//! container is not running, or not known to this backend, has no figures.

use dotcore_domain::real_time;
use dotcore_runtime::{
    WorkloadUnitResourceObservation, WorkloadUnitResources, WorkloadUnitResourcesUnavailable,
};

use super::{super::client::DockerContainerStats, *};

/// Inactive file cache under cgroup v1, whose presence marks a v1 reading.
const CGROUP_V1_INACTIVE_FILE: &str = "total_inactive_file";
/// Inactive file cache under cgroup v2.
const CGROUP_V2_INACTIVE_FILE: &str = "inactive_file";

impl<C: DockerClient> DockerAdapter<C> {
    /// Reads every requested unit's resource use from its container's Engine stats.
    pub(super) fn group_resources<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>> {
        Box::pin(async move {
            validate_group_launch(group, &self.config)?;
            // Only containers this backend created or verified for the same launch generation are
            // read, so a foreign or replaced container never lends its figures.
            let containers = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .get(&group.group_id.0)
                .filter(|known| known.launch.correlation == group.correlation)
                .map(|known| {
                    known
                        .units
                        .iter()
                        .map(|(name, unit)| (name.clone(), unit.container_id.clone()))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            let mut observations = BTreeMap::new();
            for name in group.units.keys() {
                let resources = match containers.get(name) {
                    Some(container_id) => match self.client.container_stats(container_id).await {
                        Ok(stats) => unit_resources(&stats),
                        Err(error) if error.is_not_found() => WorkloadUnitResources::Unavailable(
                            WorkloadUnitResourcesUnavailable::NotRunning,
                        ),
                        Err(error) => WorkloadUnitResources::Unavailable(
                            WorkloadUnitResourcesUnavailable::Read(
                                anyhow::Error::new(error).context("Docker container stats failed"),
                            ),
                        ),
                    },
                    None => WorkloadUnitResources::Unavailable(
                        WorkloadUnitResourcesUnavailable::NotRunning,
                    ),
                };
                observations.insert(
                    name.clone(),
                    WorkloadUnitResourceObservation {
                        observed_at: real_time::wall_now(),
                        resources,
                    },
                );
            }
            Ok(observations)
        })
    }
}

/// Maps one container's Engine stats to its unit's figures. The Engine answers a container that
/// is not running with empty stats, so a reading without memory usage means not running.
fn unit_resources(stats: &DockerContainerStats) -> WorkloadUnitResources {
    let Some(usage) = stats.memory_usage_bytes else {
        return WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::NotRunning);
    };
    let Some(cpu_time_ns) = stats.cpu_time_ns else {
        return WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::Read(
            anyhow::anyhow!("Docker container stats report memory but no CPU time"),
        ));
    };
    WorkloadUnitResources::Available {
        cpu_time_ns,
        memory_bytes: working_set(usage, stats),
    }
}

/// Charged memory without inactive file cache, as the Docker CLI computes it: cgroup v1 names the
/// cache `total_inactive_file`, cgroup v2 `inactive_file`. A cache figure that isn't below the
/// usage is ignored rather than driving the result to zero.
fn working_set(usage: u64, stats: &DockerContainerStats) -> u64 {
    let inactive_file = match stats.memory_stat.get(CGROUP_V1_INACTIVE_FILE) {
        Some(v1) => Some(*v1),
        None => stats.memory_stat.get(CGROUP_V2_INACTIVE_FILE).copied(),
    };
    match inactive_file {
        Some(inactive_file) if inactive_file < usage => usage - inactive_file,
        _ => usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_v2_memory_excludes_inactive_file_cache() {
        let resources = unit_resources(&stats(
            Some(7_000_000_000),
            Some(500),
            &[("inactive_file", 120), ("active_file", 40), ("anon", 300)],
        ));

        assert!(matches!(
            resources,
            WorkloadUnitResources::Available {
                cpu_time_ns: 7_000_000_000,
                memory_bytes: 380,
            }
        ));
    }

    #[test]
    fn cgroup_v1_memory_excludes_total_inactive_file_cache() {
        let resources = unit_resources(&stats(
            Some(42),
            Some(1_000),
            &[
                ("total_inactive_file", 250),
                ("inactive_file", 10),
                ("cache", 400),
            ],
        ));

        assert!(matches!(
            resources,
            WorkloadUnitResources::Available {
                cpu_time_ns: 42,
                memory_bytes: 750,
            }
        ));
    }

    #[test]
    fn memory_without_a_smaller_cache_figure_is_the_charged_usage() {
        let no_cache = unit_resources(&stats(Some(1), Some(900), &[]));
        let oversized_cache = unit_resources(&stats(Some(1), Some(900), &[("inactive_file", 900)]));

        assert!(matches!(
            no_cache,
            WorkloadUnitResources::Available {
                memory_bytes: 900,
                ..
            }
        ));
        assert!(matches!(
            oversized_cache,
            WorkloadUnitResources::Available {
                memory_bytes: 900,
                ..
            }
        ));
    }

    #[test]
    fn empty_stats_of_a_stopped_container_read_as_not_running() {
        let resources = unit_resources(&DockerContainerStats::default());

        assert!(matches!(
            resources,
            WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::NotRunning)
        ));
    }

    #[test]
    fn memory_without_cpu_time_is_unavailable_with_its_reason() {
        let resources = unit_resources(&stats(None, Some(900), &[]));

        let WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::Read(error)) =
            resources
        else {
            panic!("expected a read failure, got {resources:?}");
        };
        assert_eq!(
            error.to_string(),
            "Docker container stats report memory but no CPU time"
        );
    }

    fn stats(
        cpu_time_ns: Option<u64>,
        memory_usage_bytes: Option<u64>,
        memory_stat: &[(&str, u64)],
    ) -> DockerContainerStats {
        DockerContainerStats {
            cpu_time_ns,
            memory_usage_bytes,
            memory_stat: memory_stat
                .iter()
                .map(|(key, value)| ((*key).to_owned(), *value))
                .collect(),
        }
    }
}
