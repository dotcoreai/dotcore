//! Shared controller composition pieces: the registry-backed target capability source, the site
//! workload source over both workload controllers, engine integrations, hardware collection, and
//! OCI preparation.

use std::sync::Arc;

use dotcore_node_hardware::hardware::SystemHardwareSnapshotCollector;
use dotcore_proto::dotcore::{application::v1::ApplicationDeployment, site::v1::ExecutionTarget};
use dotcore_site::node::{
    registry::NodeRegistry,
    target_inventory::{
        TargetCapabilityInventory, TargetCapabilitySource, TargetCapabilitySourceError,
    },
};

use crate::{
    application::ApplicationLifecycleError,
    hardware::{
        capacity::NodeWorkloads,
        workloads::{SiteWorkloadSource, SiteWorkloadsError, join_lanes},
    },
    model::controller::DeploymentProjection,
};

/// Execution-target capabilities of every SiteNode, read from the manager's registry of node
/// observation reports.
pub(super) struct RegistryTargetCapabilitySource {
    registry: NodeRegistry,
}

impl RegistryTargetCapabilitySource {
    pub(super) fn new(registry: NodeRegistry) -> Self {
        Self { registry }
    }
}

impl TargetCapabilitySource for RegistryTargetCapabilitySource {
    fn observe(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<TargetCapabilityInventory, TargetCapabilitySourceError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(std::future::ready(self.registry.target_capabilities()))
    }
}

/// What every lane's deployments hold of each node's memory: the inference controller's, then the
/// apps controller's, each read from its projection without waiting for its lifecycle operations.
pub(super) struct ControllerWorkloads {
    models: DeploymentProjection,
    applications: Box<ApplicationDeployments>,
}

/// Reads the apps controller's projected deployments.
type ApplicationDeployments =
    dyn Fn() -> Result<Vec<ApplicationDeployment>, ApplicationLifecycleError> + Send + Sync;

impl ControllerWorkloads {
    pub(super) fn new(
        models: DeploymentProjection,
        applications: impl Fn() -> Result<Vec<ApplicationDeployment>, ApplicationLifecycleError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            models,
            applications: Box::new(applications),
        }
    }
}

impl SiteWorkloadSource for ControllerWorkloads {
    fn node_workloads(
        &self,
        inventory: Option<&TargetCapabilityInventory>,
    ) -> Result<NodeWorkloads, SiteWorkloadsError> {
        let models = self.models.deployments().map_err(SiteWorkloadsError::new)?;
        let applications = (self.applications)().map_err(SiteWorkloadsError::new)?;
        Ok(join_lanes([
            crate::model::compatibility::node_workloads(&models, inventory),
            crate::application::workloads::node_workloads(&applications, inventory),
        ]))
    }
}

/// The host's hardware collector, stamping snapshots with `clock`.
pub(super) fn system_collector(
    clock: Arc<dyn dotcore_domain::clock::Clock>,
) -> SystemHardwareSnapshotCollector {
    SystemHardwareSnapshotCollector::with_platform_accelerators(clock)
}

/// Builds the engine integration registry of a manager or worker model controller, with the
/// simulated native provider pacing on `timing` and `clock`, and a loopback transport for every
/// local engine kind.
pub(super) fn engine_integrations(
    simulated_inference: Option<crate::inference::simulated::SimulatedInferenceTestSupport>,
    timing: &dotcore_domain::timing::Timing,
    clock: Arc<dyn dotcore_domain::clock::Clock>,
) -> Arc<crate::model::engine::EngineIntegrationRegistry> {
    let provider = crate::inference::simulated::SimulatedNativeProvider::new(timing, clock);
    Arc::new(
        crate::model::engine::EngineIntegrationRegistry::with_native_provider_and_transports(
            Arc::new(match simulated_inference {
                Some(test_support) => provider.with_test_support(test_support),
                None => provider,
            }),
            crate::model::engine::EngineTransportFactories::local_engines(),
        ),
    )
}

#[cfg(test)]
pub(super) fn default_collector() -> SystemHardwareSnapshotCollector {
    system_collector(Arc::new(dotcore_domain::clock::SystemClock))
}

pub(super) async fn oci_preparer_for<C>(
    targets: &[ExecutionTarget],
    connect: impl FnOnce() -> C,
) -> Result<
    Option<Arc<dyn dotcore_artifact::OciImagePreparer>>,
    crate::model::controller::ModelLifecycleError,
>
where
    C: Future<Output = Result<dotcore_artifact::DockerOciClient, dotcore_artifact::OciClientError>>,
{
    if !targets.iter().any(|target| {
        matches!(
            target.runtime_binding,
            Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(
                dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                    adapter: Some(dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter::DockerRuntime(_)),
                }
            ))
        )
    })
    {
        return Ok(None);
    }
    let client = connect().await.map_err(|source| {
        crate::model::controller::ModelLifecycleError::Configuration(
            anyhow::Error::new(source).context("local Docker OCI client initialization failed"),
        )
    })?;
    Ok(Some(Arc::new(dotcore_artifact::OciPreparer::new(
        Arc::new(client),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_and_worker_engine_registry_binds_ninfer_loopback_services() {
        let launch = crate::model::resolved::ResolvedModelLaunch::from_snapshot(
            &crate::test_support::ninfer::local_model_snapshot("qwen3-8-27b"),
        )
        .expect("ninfer launch");
        let service = dotcore_runtime::WorkloadServiceBinding::loopback(
            "ninfer-workload",
            "http",
            1,
            dotcore_runtime::LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: 18080,
            },
        );

        engine_integrations(
            None,
            &dotcore_domain::timing::Timing::production(),
            Arc::new(dotcore_domain::clock::SystemClock),
        )
        .bind_workload(&launch, "deployment", &service)
        .expect("ninfer loopback binding");
        assert_eq!(
            crate::model::engine::EngineIntegrationRegistry::production()
                .bind_workload(&launch, "deployment", &service)
                .err()
                .expect("no ninfer transport")
                .to_string(),
            crate::inference::EngineBindingError::Unavailable(
                "ninfer transport factory is not registered".to_owned()
            )
            .to_string()
        );
    }
}
