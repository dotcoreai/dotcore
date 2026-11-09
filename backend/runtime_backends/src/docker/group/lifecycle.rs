//! Runtime trait dispatch for Docker workload groups.

use std::collections::BTreeMap;

use dotcore_runtime::{
    ReadyLaunchGateOutcome, RuntimeBackend, RuntimeBackendError, RuntimeBackendId, RuntimeFuture,
    RuntimeGroupLogFollowQuery, RuntimeLogFuture, RuntimeLogSourceHistory,
    RuntimeLogSourceSelection, RuntimeLogSourceStream, WorkloadDependencyCondition,
    WorkloadGroupBackend, WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadUnitName,
    WorkloadUnitResourceObservation, WorkloadUnitState,
};
use tracing::{debug, warn};

use super::super::storage::materialize_storage;
use super::creation::topological_order;
use super::{
    DockerAdapter, DockerClient, DockerGroupStartError, KnownGroup, OwnedWorkloadGroupRealization,
    reverse_topological_order, runtime_error, validate_group_launch,
    verification::group_container_has_expected_ownership,
};

impl<C: DockerClient> WorkloadGroupBackend for DockerAdapter<C> {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(crate::docker::adapter::BACKEND_ID.to_owned())
    }

    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::ExecutionPlatformUnavailable,
        )))
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        self.discover_groups()
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        group_id: &'a dotcore_runtime::WorkloadGroupId,
        owner_id: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        self.ensure_group_absent(group_id, owner_id)
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.create_group(group)
    }

    fn start_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.start_group(group)
    }

    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.inspect_group(group)
    }

    fn observe_group_resources<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>> {
        self.group_resources(group)
    }

    fn remove_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        self.remove_group(group)
    }

    fn stop_workload_group<'a>(&'a self, group: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        self.stop_group(group)
    }

    fn read_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        sources: &'a RuntimeLogSourceSelection,
        query: &'a dotcore_runtime::RuntimeLogQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceHistory>> {
        Box::pin(self.read_group_logs(group, sources, query))
    }

    fn follow_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        sources: &'a RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceStream>> {
        Box::pin(self.follow_group_logs(group, sources, query))
    }
}

impl<C: DockerClient> DockerAdapter<C> {
    async fn read_group_logs(
        &self,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
        query: &dotcore_runtime::RuntimeLogQuery,
    ) -> Result<Vec<RuntimeLogSourceHistory>, RuntimeBackendError> {
        tokio::time::timeout(super::super::client::DOCKER_OPERATION_TIMEOUT, async {
            let Some(known) = self.exact_log_group(group, sources)? else {
                return Ok(sources
                    .sources()
                    .iter()
                    .map(|source| RuntimeLogSourceHistory {
                        source: source.clone(),
                        result: Err(RuntimeBackendError::workload_not_created()),
                    })
                    .collect());
            };
            let candidate_bytes =
                super::super::logs::HISTORY_CANDIDATE_BYTES / sources.sources().len().max(1);
            let mut histories = Vec::with_capacity(sources.sources().len());
            for source in sources.sources() {
                let unit = known
                    .units
                    .get(&WorkloadUnitName(source.name.clone()))
                    .expect("validated selected group unit");
                let result = match self.exact_group_log_container(unit).await {
                    Ok(container_id) => {
                        self.read_exact_logs(container_id, source, query, candidate_bytes)
                            .await
                    }
                    Err(error) => Err(error),
                };
                histories.push(RuntimeLogSourceHistory {
                    source: source.clone(),
                    result,
                });
            }
            Ok(histories)
        })
        .await
        .map_err(|source| {
            super::super::runtime_error_with_source(
                "Docker group log history acquisition timed out",
                super::super::client::ClientError::Timeout {
                    operation: "Docker group log history acquisition",
                    source,
                },
            )
        })?
    }

    async fn follow_group_logs(
        &self,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> Result<Vec<RuntimeLogSourceStream>, RuntimeBackendError> {
        let Some(known) = self.exact_log_group(group, sources)? else {
            return Ok(sources
                .sources()
                .iter()
                .map(|source| RuntimeLogSourceStream {
                    source: source.clone(),
                    result: Err(RuntimeBackendError::workload_not_created()),
                })
                .collect());
        };
        let mut streams = Vec::with_capacity(sources.sources().len());
        for source in sources.sources() {
            let unit = known
                .units
                .get(&WorkloadUnitName(source.name.clone()))
                .expect("validated selected group unit");
            let container = tokio::select! {
                _ = query.cancellation().cancelled() => return Err(RuntimeBackendError::LogCancelled),
                result = self.exact_group_log_container(unit) => result,
            };
            let result = match container {
                Ok(container_id) => self.follow_exact_logs(
                    container_id,
                    source.clone(),
                    query.resume_timestamp(&source.name),
                    query.cancellation().clone(),
                ),
                Err(error) => Err(error),
            };
            streams.push(RuntimeLogSourceStream {
                source: source.clone(),
                result,
            });
        }
        if query.cancellation().is_cancelled() {
            return Err(RuntimeBackendError::LogCancelled);
        }
        Ok(streams)
    }

    /// Returns the known group whose logs are read, or `None` while this runtime has not created
    /// it yet, as while its images are prepared.
    fn exact_log_group(
        &self,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
    ) -> Result<Option<KnownGroup>, RuntimeBackendError> {
        validate_group_launch(group, &self.config)?;
        if sources.sources().iter().any(|source| {
            !group
                .units
                .contains_key(&WorkloadUnitName(source.name.clone()))
        }) {
            return Err(runtime_error(
                "Docker workload group log source is not a declared unit",
            ));
        }
        let Some(known) = self
            .known_groups()
            .lock()
            .expect("Docker group registry")
            .get(&group.group_id.0)
            .cloned()
        else {
            debug!(event_kind = "docker_group_logs_not_created", group_id = %group.group_id.0, "Docker workload group logs requested before the group was created");
            return Ok(None);
        };
        if known.launch != *group {
            return Err(runtime_error(
                "Docker workload group identity changed before log access",
            ));
        }
        Ok(Some(known))
    }

    async fn exact_group_log_container(
        &self,
        unit: &super::types::KnownGroupUnit,
    ) -> Result<String, RuntimeBackendError> {
        let ownership = self
            .client
            .inspect_group_container_ownership(&unit.container_id)
            .await
            .map_err(|error| {
                super::super::runtime_error_with_source(
                    "Docker group log source ownership inspection failed",
                    error,
                )
            })?;
        if !group_container_has_expected_ownership(&ownership, unit) {
            return Err(runtime_error(
                "Docker workload group log source ownership changed",
            ));
        }
        Ok(unit.container_id.clone())
    }

    pub(super) fn start_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            validate_group_launch(group, &self.config)?;
            let prepared = super::prepare_group(group, &self.config)?;
            self.validate_launch_constraints(group, &prepared).await?;
            let known = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .get(&group.group_id.0)
                .cloned();
            let Some(mut known) = known.or(self.known_group_from_launch(group).await?) else {
                return Ok(WorkloadGroupObservation::Absent {
                    group_id: group.group_id.clone(),
                    correlation: group.correlation.clone(),
                    backend_id: RuntimeBackend::backend_id(self),
                });
            };
            let observation = self.retained_group_observation(&known, group).await?;
            let WorkloadGroupObservation::Observed(observed) = observation else {
                return Ok(observation);
            };
            if observed
                .units
                .values()
                .any(|unit| unit.state == WorkloadUnitState::Absent)
            {
                return Ok(WorkloadGroupObservation::Observed(observed));
            }
            // Application-scoped folders are shared across groups and may be removed while this
            // group is stopped.
            materialize_storage(
                &self.config,
                group
                    .units
                    .values()
                    .flat_map(|unit| &unit.storage)
                    .map(Into::into),
            )?;
            let mut started = Vec::new();
            for name in topological_order(group)? {
                let state = observed.units[&name].state;
                if state == WorkloadUnitState::Running {
                    if known.units[&name].gate != Some(ReadyLaunchGateOutcome::Passed)
                        && let Some(gate) = group.units[&name].ready_launch_gate.as_ref()
                    {
                        let unit = known
                            .units
                            .get(&name)
                            .expect("retained verified group unit");
                        let outcome = match self
                            .wait_ready(&unit.container_id, &unit.request, gate)
                            .await
                        {
                            Ok(outcome) => outcome,
                            Err(source) => {
                                return Err(self
                                    .rollback_started_units(&known, &started, source)
                                    .await);
                            }
                        };
                        known
                            .units
                            .get_mut(&name)
                            .expect("retained verified group unit")
                            .gate = Some(outcome);
                        self.known_groups()
                            .lock()
                            .expect("Docker group registry")
                            .insert(group.group_id.0.clone(), known.clone());
                    }
                    continue;
                }
                if state != WorkloadUnitState::Stopped {
                    let source = runtime_error(
                        "Docker reversible group unit has an unsupported lifecycle state",
                    );
                    return Err(self.rollback_started_units(&known, &started, source).await);
                }
                let dependencies_satisfied =
                    group.units[&name].dependencies.iter().all(|dependency| {
                        let unit = known.units.get(&dependency.unit_name);
                        dependency.condition == WorkloadDependencyCondition::Started
                            || unit.is_some_and(|unit| {
                                unit.gate == Some(ReadyLaunchGateOutcome::Passed)
                            })
                    });
                if !dependencies_satisfied {
                    continue;
                }
                let unit = known
                    .units
                    .get(&name)
                    .expect("retained verified group unit");
                if let Err(error) = self.client.start_container(&unit.container_id).await {
                    let source = super::creation::group_unit_start_error(&name, error);
                    if source.is_published_port_in_use() {
                        self.discard_unstartable_unit(group, &name, &unit.container_id)
                            .await;
                    }
                    return Err(self.rollback_started_units(&known, &started, source).await);
                }
                started.push(name.clone());
                let gate = match group.units[&name].ready_launch_gate.as_ref() {
                    Some(gate) => match self
                        .wait_ready(&unit.container_id, &unit.request, gate)
                        .await
                    {
                        Ok(outcome) => Some(outcome),
                        Err(source) => {
                            return Err(self
                                .rollback_started_units(&known, &started, source)
                                .await);
                        }
                    },
                    None => None,
                };
                known
                    .units
                    .get_mut(&name)
                    .expect("retained verified group unit")
                    .gate = gate;
                self.known_groups()
                    .lock()
                    .expect("Docker group registry")
                    .insert(group.group_id.0.clone(), known.clone());
            }
            self.retained_group_observation(&known, group).await
        })
    }

    /// Removes a unit container whose start failed while Docker set up its host port. Docker
    /// detaches such a container from its networks for good, so it can never serve again; once
    /// removed, the group reports the unit absent and its owner recreates the group.
    async fn discard_unstartable_unit(
        &self,
        group: &WorkloadGroupLaunch,
        name: &WorkloadUnitName,
        container_id: &str,
    ) {
        match self.client.remove_container(container_id).await {
            Ok(()) => {}
            Err(error) if error.is_not_found() => {}
            Err(error) => {
                warn!(event_kind = "docker_group_unit_discard", deployment_id = %group.correlation.owner_id, group_id = %group.group_id.0, unit = %name.0, outcome = "failed", error = %error, "Docker group unit that failed to bind its published port could not be removed");
                return;
            }
        }
        if let Some(known) = self
            .known_groups()
            .lock()
            .expect("Docker group registry")
            .get_mut(&group.group_id.0)
        {
            known.units.remove(name);
        }
    }

    async fn rollback_started_units(
        &self,
        known: &KnownGroup,
        started: &[WorkloadUnitName],
        start_error: RuntimeBackendError,
    ) -> RuntimeBackendError {
        if started.is_empty() {
            return start_error;
        }
        let mut rollback_error = None;
        for name in started.iter().rev() {
            let unit = known.units.get(name).expect("started retained group unit");
            if let Err(error) = self.client.stop_container(&unit.container_id).await
                && !error.is_not_found()
                && !error.is_not_modified()
                && rollback_error.is_none()
            {
                rollback_error = Some(super::runtime_error_with_source(
                    format!("Docker group unit start rollback failed for {}", name.0),
                    error,
                ));
            }
        }
        let Some(rollback_error) = rollback_error else {
            return start_error;
        };
        DockerGroupStartError::new(start_error, rollback_error, owned_realization(known)).into()
    }

    pub(super) fn stop_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            validate_group_launch(group, &self.config)?;
            let known = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .get(&group.group_id.0)
                .cloned();
            let Some(known) = known.or(self.known_group_from_launch(group).await?) else {
                return Err(runtime_error("Docker workload group is absent before stop"));
            };
            let observation = self.retained_group_observation(&known, group).await?;
            let WorkloadGroupObservation::Observed(observed) = observation else {
                return Err(runtime_error(
                    "Docker workload group is stale before reversible stop",
                ));
            };
            for name in reverse_topological_order(group)? {
                if observed.units[&name].state != WorkloadUnitState::Running {
                    continue;
                }
                let unit = known
                    .units
                    .get(&name)
                    .expect("observed retained group unit");
                self.client
                    .stop_container(&unit.container_id)
                    .await
                    .map_err(|error| {
                        super::runtime_error_with_source(
                            format!("Docker group unit stop failed for {}", name.0),
                            error,
                        )
                    })?;
            }
            Ok(())
        })
    }
}

fn owned_realization(known: &KnownGroup) -> OwnedWorkloadGroupRealization {
    OwnedWorkloadGroupRealization {
        group_id: known.launch.group_id.clone(),
        network_id: known.network_id.clone(),
        units: known
            .units
            .iter()
            .map(|(name, unit)| (name.clone(), unit.container_id.clone()))
            .collect(),
    }
}
