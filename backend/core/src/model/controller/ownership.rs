//! Checks that the identity a runtime attaches to a workload belongs to the
//! model deployment the controller expects.
//!
//! Every identity is checked during construction, reconciliation, and
//! teardown before any lifecycle action. Identities whose execution target,
//! backend, workload ID, or deployment correlation conflicts with the desired
//! site declaration are rejected, so the controller never stops, inspects,
//! or reads logs from a workload it does not own.

use std::collections::BTreeSet;

use dotcore_artifact::ArtifactManager;
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::{model::v1::ModelDeploymentDeclaration, site::v1::Site};
use dotcore_runtime::{MAX_WORKLOAD_ID_BYTES, ObservedWorkload, WorkloadId, WorkloadObservation};

use super::{
    ModelLifecycleError,
    launch::{execution_target, resolved_launch, selected_target, workload_launch},
    state::{configuration, desired_state, observation_identity},
};
use crate::dispatch::model::{RuntimeTargetMap, TargetObservation};

pub(super) fn validate_desired(
    site: &Site,
    runtimes: &RuntimeTargetMap,
) -> Result<(), ModelLifecycleError> {
    for desired in &site.model_deployments {
        desired_state(desired)?;
        let selected_target = selected_target(site, desired)?;
        execution_target(selected_target)?;
        runtimes.backend_id(
            &desired
                .placement
                .as_ref()
                .ok_or_else(|| configuration("local model deployment has no exact placement"))?
                .execution_target_id,
        )?;
    }
    Ok(())
}

pub(super) fn validate_observations(
    site: &Site,
    runtimes: &RuntimeTargetMap,
    observations: &[TargetObservation],
) -> Result<(), ModelLifecycleError> {
    let mut deployment_ids = BTreeSet::new();
    let mut workload_ids = BTreeSet::new();
    for target_observation in observations {
        let Some(identity) = observation_identity(&target_observation.observation) else {
            return Err(configuration(
                "Runtime discovery returned an inconclusive observation",
            ));
        };
        validate_observed_identity(runtimes, &target_observation.execution_target_id, identity)?;
        if !deployment_ids.insert(identity.correlation.owner_id.clone())
            || !workload_ids.insert((
                target_observation.execution_target_id.clone(),
                identity.workload_id.0.clone(),
            ))
        {
            return Err(configuration(
                "Runtime discovery returned conflicting or duplicate ownership",
            ));
        }
        if let Some(desired) = site
            .model_deployments
            .iter()
            .find(|value| value.model_deployment_id == identity.correlation.owner_id)
        {
            validate_discovered_observation(
                site,
                runtimes,
                desired,
                &target_observation.execution_target_id,
                identity,
            )?;
        }
    }
    Ok(())
}

fn validate_discovered_observation(
    site: &Site,
    runtimes: &RuntimeTargetMap,
    desired: &ModelDeploymentDeclaration,
    execution_target_id: &str,
    identity: &ObservedWorkload,
) -> Result<(), ModelLifecycleError> {
    let expected_backend = runtimes.backend_id(
        &desired
            .placement
            .as_ref()
            .ok_or_else(|| configuration("local model deployment has no exact placement"))?
            .execution_target_id,
    )?;
    if execution_target_id
        != desired
            .placement
            .as_ref()
            .ok_or_else(|| configuration("local model deployment has no exact placement"))?
            .execution_target_id
        || identity.correlation.owner_id != desired.model_deployment_id
        || identity.backend_id != expected_backend
        || selected_target(site, desired)?.execution_target_id
            != desired
                .placement
                .as_ref()
                .ok_or_else(|| configuration("local model deployment has no exact placement"))?
                .execution_target_id
    {
        return Err(configuration(
            "Runtime returned ownership evidence that conflicts with the desired deployment",
        ));
    }
    Ok(())
}

pub(super) fn validate_action_observation(
    site: &Site,
    runtimes: &RuntimeTargetMap,
    artifacts: &ArtifactManager,
    desired: &ModelDeploymentDeclaration,
    execution_target_id: &str,
    expected_workload_id: Option<&WorkloadId>,
    observation: &WorkloadObservation,
) -> Result<(), ModelLifecycleError> {
    let identity = observation_identity(observation).ok_or_else(|| {
        configuration("Runtime action returned an inconclusive ownership observation")
    })?;
    validate_observed_identity(runtimes, execution_target_id, identity)?;
    validate_expected_observation(
        site,
        runtimes,
        artifacts,
        desired,
        execution_target_id,
        expected_workload_id,
        identity,
    )
}

/// Validates a teardown action's observation by owner identity alone: the workload `expected`
/// observed before the action, on the deployment's placement target and runtime backend. The
/// current launch is never derived, so a deployment whose launch no longer resolves, or whose
/// workload was launched under an older launch, can still be torn down.
pub(super) fn validate_teardown_observation(
    site: &Site,
    runtimes: &RuntimeTargetMap,
    desired: &ModelDeploymentDeclaration,
    execution_target_id: &str,
    expected: &ObservedWorkload,
    observation: &WorkloadObservation,
) -> Result<(), ModelLifecycleError> {
    let identity = observation_identity(observation).ok_or_else(|| {
        configuration("Runtime action returned an inconclusive ownership observation")
    })?;
    validate_observed_identity(runtimes, execution_target_id, identity)?;
    validate_discovered_observation(site, runtimes, desired, execution_target_id, identity)?;
    if identity.workload_id != expected.workload_id || identity.correlation != expected.correlation
    {
        return Err(configuration(
            "Runtime returned ownership evidence that conflicts with the desired deployment",
        ));
    }
    Ok(())
}

pub(super) fn validate_observed_identity(
    runtimes: &RuntimeTargetMap,
    execution_target_id: &str,
    identity: &ObservedWorkload,
) -> Result<(), ModelLifecycleError> {
    let expected_backend = runtimes.backend_id(execution_target_id)?;
    if !valid_id(execution_target_id)
        || !valid_id(&identity.correlation.owner_id)
        || !valid_workload_id(&identity.workload_id.0)
        || !valid_runtime_backend_id(&identity.backend_id.0)
        || !valid_runtime_backend_id(&expected_backend.0)
        || identity.correlation.reconcile_generation == 0
        || identity.backend_id != expected_backend
    {
        return Err(configuration(
            "Runtime returned malformed or mismatched ownership evidence",
        ));
    }
    Ok(())
}

fn validate_expected_observation(
    site: &Site,
    runtimes: &RuntimeTargetMap,
    artifacts: &ArtifactManager,
    desired: &ModelDeploymentDeclaration,
    execution_target_id: &str,
    expected_workload_id: Option<&WorkloadId>,
    identity: &ObservedWorkload,
) -> Result<(), ModelLifecycleError> {
    let target = selected_target(site, desired)?;
    let resolved = resolved_launch(desired)?;
    let artifacts = artifacts.expected_model_paths(&resolved.artifacts)?;
    let launch = workload_launch(desired, target, &artifacts)?;
    let backend = runtimes.backend_id(
        &desired
            .placement
            .as_ref()
            .ok_or_else(|| configuration("local model deployment has no exact placement"))?
            .execution_target_id,
    )?;
    if execution_target_id
        != desired
            .placement
            .as_ref()
            .ok_or_else(|| configuration("local model deployment has no exact placement"))?
            .execution_target_id
        || identity.correlation != launch.correlation
        || identity.backend_id != backend
        || expected_workload_id.is_some_and(|expected| identity.workload_id != *expected)
    {
        return Err(configuration(
            "Runtime returned ownership evidence that conflicts with the desired deployment",
        ));
    }
    Ok(())
}

fn valid_runtime_backend_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_whitespace())
}

fn valid_workload_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_WORKLOAD_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use dotcore_artifact::ArtifactManager;
    use dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState;
    use dotcore_runtime::{
        MAX_WORKLOAD_ID_BYTES, ObservedWorkload, RuntimeBackendId, WorkloadCorrelation, WorkloadId,
        WorkloadObservation,
    };

    use super::{validate_action_observation, validate_observations, validate_observed_identity};
    use crate::{
        dispatch::model::TargetObservation,
        model::controller::{
            construction::validate_runtime_inputs,
            test_support::{
                expected_identity, test_desired_deployment, test_runtime_target, test_site,
            },
        },
    };

    #[test]
    fn controller_accepts_the_longest_site_scoped_docker_workload_identity() {
        let site_id = "a".repeat(63);
        let deployment_id = "b".repeat(63);
        let identity = ObservedWorkload {
            workload_id: WorkloadId(format!("dotcore_{site_id}_{deployment_id}_{}", u64::MAX)),
            correlation: WorkloadCorrelation {
                owner_id: deployment_id,
                reconcile_generation: u64::MAX,
            },
            backend_id: RuntimeBackendId("runtime/test".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        };

        assert_eq!(identity.workload_id.0.len(), MAX_WORKLOAD_ID_BYTES);
        validate_observed_identity(&test_runtime_target(), "target-one", &identity)
            .expect("maximum site-scoped Docker identity");
    }

    #[test]
    fn discovery_rejects_malformed_or_mismatched_runtime_ownership_evidence() {
        let mut site = test_site();
        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        site.model_deployments.push(desired.clone());
        let runtimes = test_runtime_target();
        let identity = expected_identity(&site, &desired, "runtime/test");

        for observation in [
            TargetObservation {
                execution_target_id: "missing-target".to_owned(),
                observation: WorkloadObservation::Running(identity.clone()),
            },
            TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(ObservedWorkload {
                    backend_id: RuntimeBackendId("runtime/other".to_owned()),
                    ..identity.clone()
                }),
            },
            TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(ObservedWorkload {
                    workload_id: WorkloadId("invalid workload".to_owned()),
                    ..identity.clone()
                }),
            },
            TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(ObservedWorkload {
                    correlation: WorkloadCorrelation {
                        reconcile_generation: 0,
                        ..identity.correlation.clone()
                    },
                    ..identity.clone()
                }),
            },
        ] {
            assert!(validate_runtime_inputs(&site, &runtimes, &[observation]).is_err());
        }
    }

    #[test]
    fn direct_action_observations_require_the_expected_ownership_evidence() {
        let mut site = test_site();
        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        site.model_deployments.push(desired.clone());
        let runtimes = test_runtime_target();
        let artifacts = ArtifactManager::new("/test-cache".into(), None).expect("artifact manager");
        let identity = expected_identity(&site, &desired, "runtime/test");

        validate_action_observation(
            &site,
            &runtimes,
            &artifacts,
            &desired,
            "target-one",
            Some(&identity.workload_id),
            &WorkloadObservation::Running(identity.clone()),
        )
        .expect("matching action observation");
        assert!(
            validate_action_observation(
                &site,
                &runtimes,
                &artifacts,
                &desired,
                "target-one",
                Some(&identity.workload_id),
                &WorkloadObservation::Running(ObservedWorkload {
                    workload_id: WorkloadId("other-workload".to_owned()),
                    ..identity
                }),
            )
            .is_err()
        );
    }

    #[test]
    fn direct_action_observations_accept_configured_artifact_cache_paths() {
        let mut site = test_site();
        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        site.model_deployments.push(desired.clone());
        let artifacts = ArtifactManager::new("/configured-artifact-cache".into(), None)
            .expect("artifact manager");
        let resolved =
            crate::model::controller::launch::resolved_launch(&desired).expect("resolved model");
        let prepared = artifacts
            .expected_model_paths(&resolved.artifacts)
            .expect("configured artifact paths");
        let launch = crate::model::controller::launch::workload_launch(
            &desired,
            crate::model::controller::launch::selected_target(&site, &desired)
                .expect("execution target"),
            &prepared,
        )
        .expect("workload launch");
        let identity = ObservedWorkload {
            workload_id: WorkloadId("configured-cache-workload".to_owned()),
            correlation: launch.correlation,
            backend_id: RuntimeBackendId("runtime/test".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        };

        validate_action_observation(
            &site,
            &test_runtime_target(),
            &artifacts,
            &desired,
            "target-one",
            Some(&identity.workload_id),
            &WorkloadObservation::Running(identity.clone()),
        )
        .expect("matching configured-cache action observation");
    }

    #[test]
    fn observation_validation_rejects_duplicate_owned_workloads() {
        let site = test_site();
        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        let identity = expected_identity(&site, &desired, "runtime/test");
        let observations = [
            TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(identity.clone()),
            },
            TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(identity),
            },
        ];

        assert!(validate_observations(&site, &test_runtime_target(), &observations).is_err());
    }
}
