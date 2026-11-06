//! Test entry points of the backend library, compiled only with the `test-support` feature, so a
//! production build carries none of them.
//!
//! Integration and command-line tests reach every test-only control through this one module: the
//! manual and virtual clocks a composed backend reads, its timing policy, the view of a site
//! manager's node liveness, the relaxed file flush, a fake host-name resolver, simulated inference
//! holds, the profile compatibility read, and the preparation output a node retains. Each item stays defined beside the code it controls.

pub use dotcore_domain::{
    clock::{ManualClock, VirtualClock},
    fs_sync::relax_sync_for_tests,
    test_support::{
        host_resolver::FakeHostResolver,
        ports::{ReservedPort, reserve_port},
    },
    timing::Timing,
};
pub use dotcore_site::node::control::NodeLivenessAges;

pub use crate::{composition::NodeLiveness, inference::simulated::SimulatedInferenceTestSupport};

/// Evaluates every profile of `models` on `targets` through the profile compatibility read, with
/// an optional sizing scenario, at the current time.
pub fn profile_compatibility(
    catalog: &dotcore_catalog::Catalog,
    models: &[dotcore_proto::dotcore::catalog::v1::Model],
    targets: &[dotcore_proto::dotcore::site::v1::ExecutionTarget],
    scenario: Option<&crate::model::sizing_scenario::SizingScenario>,
    inventory: &dotcore_site::node::target_inventory::TargetCapabilityInventory,
) -> dotcore_proto::dotcore::model::v1::ListModelProfileCompatibilityResponse {
    crate::model::compatibility::evaluate_profile_compatibility(
        catalog,
        models,
        targets,
        scenario,
        Some(inventory),
        &Default::default(),
        std::time::SystemTime::now(),
    )
}

/// Declarations of the metrics each writer exports, in the order node hardware, forwarder, router,
/// model deployments and application deployments, for a site collecting hardware every
/// `refresh_interval`. The composed catalog must contain every one.
pub fn writer_metric_descriptors(
    refresh_interval: std::time::Duration,
) -> Vec<dotcore_observability::MetricDescriptor> {
    let mut descriptors = dotcore_site::node::metrics::hardware_metric_descriptors();
    descriptors.extend(dotcore_observability::forwarder_metric_descriptors());
    descriptors.extend(crate::router::metrics::router_metric_descriptors());
    descriptors
        .extend(crate::model::controller::metrics::deployment_metric_descriptors(refresh_interval));
    descriptors.extend(crate::application::application_metric_descriptors(
        refresh_interval,
    ));
    descriptors
}

/// Appends one stdout line to a preparation step of a deployment, in the node store at
/// `node_store`, as the hosting node does while it prepares the deployment. `step_source` is the
/// step's log source name.
pub fn append_preparation_log(
    node_store: &std::path::Path,
    deployment_id: &str,
    step_source: &str,
    time: std::time::SystemTime,
    line: &str,
) {
    let step = crate::deployment_logs::step_from_source_name(step_source)
        .expect("a preparation step source name");
    crate::deployment_logs::PreparationLogs::on_system_clock(node_store)
        .writer(deployment_id, step)
        .append(
            time,
            dotcore_runtime::RuntimeLogStreamKind::Stdout,
            line.as_bytes(),
        );
}
