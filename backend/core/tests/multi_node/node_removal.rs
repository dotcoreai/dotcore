//! Proves a manager restarts cleanly after its worker node is removed offline, that the removed
//! worker can no longer report to it, and that a node of the same name joins again and serves a
//! model. A second scenario removes only the worker's pin and proves the manager still starts
//! and serves its own deployments.
//!
//! Both scenarios use two real processes with generated worker custody and mutual TLS, and the
//! simulated model runtime, so they need a host that observes an NVIDIA driver.

#![cfg(target_os = "linux")]

use std::fs;

use dotcore::model::engine::declaration_validator;
use dotcore_proto::dotcore::site::v1::NodeObservationState;
use dotcore_site::{
    initial::validation::SiteInputBundleValidator,
    manager_trust::{SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    membership::{NodeRemoval, OfflineSite},
};
use support::{
    TwoProcessObservationHarness, WORKER_DISPLAY_NAME, WORKER_NODE_ID, chat_status,
    listed_deployments, listed_routes, machine_ids, machine_states, place_model, wait_for_ready,
};

use crate::support;

/// Refusal the manager's private listener returns to a peer it cannot authenticate.
const AUTHENTICATION_REFUSAL: &str = "private peer authentication failed";
/// Failure a worker reports when the manager opens no private listener to connect to.
const CONNECTION_FAILURE: &str = "tcp connect error";
const WORKER_ROUTE: &str = "worker-model";
const MANAGER_ROUTE: &str = "manager-model";
const MANAGER_NODE_ID: &str = "fixture-node";
const MANAGER_MODEL_TARGET_ID: &str = "fixture-target";

#[tokio::test]
async fn a_removed_worker_is_forgotten_by_the_restarted_manager_and_can_rejoin() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(mut harness) = TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let manager_root = harness.manager().site_root().to_owned();
    let pin = manager_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY)
        .join(format!("{WORKER_NODE_ID}.sha256"));

    // A model on the worker, exposed through a route, serves before the removal.
    let deployment_id = place_model(
        manager_address,
        WORKER_NODE_ID,
        support::WORKER_MODEL_TARGET_ID,
        WORKER_ROUTE,
    )
    .await;
    wait_for_ready(manager_address, &deployment_id).await;
    assert_eq!(
        listed_deployments(manager_address).await,
        [deployment_id.as_str()]
    );
    assert_eq!(listed_routes(manager_address).await, [WORKER_ROUTE]);
    assert_eq!(
        chat_status(&harness, WORKER_ROUTE).await,
        reqwest::StatusCode::OK
    );
    assert!(pin.is_file());

    // Offline removal with forget, on the stopped site.
    harness.manager_mut().shutdown();
    harness.worker_mut().stop();
    let removal = {
        let site = OfflineSite::open(
            &manager_root,
            &SiteInputBundleValidator::new(
                declaration_validator(),
                dotcore::model::site_declaration_rules(),
            ),
        )
        .expect("open the stopped site");
        let NodeRemoval::Planned(plan) = site
            .plan_node_removal(WORKER_DISPLAY_NAME, true)
            .expect("plan the removal")
        else {
            panic!("the worker is declared");
        };
        site.remove_node(&plan).expect("remove the worker");
        plan
    };
    assert_eq!(
        removal
            .forgotten_deployments
            .iter()
            .map(|deployment| deployment.id.as_str())
            .collect::<Vec<_>>(),
        [deployment_id.as_str()]
    );
    assert_eq!(
        removal
            .deleted_routes
            .iter()
            .map(|route| route.name.as_str())
            .collect::<Vec<_>>(),
        [WORKER_ROUTE]
    );
    assert!(!pin.exists());
    assert_eq!(declared_node_ids(&harness), [MANAGER_NODE_ID]);

    // The manager starts, and the deployment and its route are gone.
    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    assert_eq!(
        listed_deployments(manager_address).await,
        Vec::<String>::new()
    );
    assert_eq!(listed_routes(manager_address).await, Vec::<String>::new());
    assert_eq!(machine_ids(manager_address).await, [MANAGER_NODE_ID]);
    assert_eq!(
        chat_status(&harness, WORKER_ROUTE).await,
        reqwest::StatusCode::NOT_FOUND
    );

    // The old worker folder starts but cannot connect: a manager whose site has no other node
    // opens no private listener. The missing-pin scenario proves the pin check refusing it.
    let worker_root = harness.worker().site_root().to_owned();
    let refused_before = support::refused_stream_opens(&worker_root, CONNECTION_FAILURE);
    harness.worker_mut().restart();
    support::wait_for_refused_stream_opens(&worker_root, CONNECTION_FAILURE, refused_before + 1)
        .await;
    assert_eq!(machine_ids(manager_address).await, [MANAGER_NODE_ID]);
    assert_eq!(
        listed_deployments(manager_address).await,
        Vec::<String>::new()
    );

    // A node of the same name joins a fresh folder and serves a model again.
    harness.manager_mut().shutdown();
    harness.worker_mut().stop();
    let joined = harness.rejoin_worker();
    assert_eq!(joined.node_id, WORKER_NODE_ID);
    assert!(pin.is_file());
    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let rejoined_deployment_id = place_model(
        manager_address,
        WORKER_NODE_ID,
        support::WORKER_MODEL_TARGET_ID,
        WORKER_ROUTE,
    )
    .await;
    assert_ne!(rejoined_deployment_id, deployment_id);
    wait_for_ready(manager_address, &rejoined_deployment_id).await;
    assert_eq!(
        listed_deployments(manager_address).await,
        [rejoined_deployment_id.as_str()]
    );
    assert_eq!(listed_routes(manager_address).await, [WORKER_ROUTE]);
    assert_eq!(
        chat_status(&harness, WORKER_ROUTE).await,
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn a_declared_worker_without_its_pin_cannot_authenticate_and_the_manager_keeps_serving() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(mut harness) = TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    let manager_root = harness.manager().site_root().to_owned();

    let deployment_id = place_model(
        manager_address,
        MANAGER_NODE_ID,
        MANAGER_MODEL_TARGET_ID,
        MANAGER_ROUTE,
    )
    .await;
    wait_for_ready(manager_address, &deployment_id).await;
    assert_eq!(
        chat_status(&harness, MANAGER_ROUTE).await,
        reqwest::StatusCode::OK
    );

    // Only the pin goes, as after a removal that stopped halfway.
    harness.manager_mut().shutdown();
    harness.worker_mut().stop();
    fs::remove_file(
        manager_root
            .join(SITE_TRUST_DIRECTORY)
            .join(WORKER_PIN_DIRECTORY)
            .join(format!("{WORKER_NODE_ID}.sha256")),
    )
    .expect("remove the worker pin");
    assert_eq!(
        declared_node_ids(&harness),
        [MANAGER_NODE_ID, WORKER_NODE_ID]
    );

    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    let worker_root = harness.worker().site_root().to_owned();
    let refused_before = support::refused_stream_opens(&worker_root, AUTHENTICATION_REFUSAL);
    harness.worker_mut().restart();
    wait_for_ready(manager_address, &deployment_id).await;
    support::wait_for_refused_stream_opens(
        &worker_root,
        AUTHENTICATION_REFUSAL,
        refused_before + 1,
    )
    .await;

    let machines = machine_states(manager_address).await;
    assert_eq!(
        machines
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [MANAGER_NODE_ID, WORKER_NODE_ID]
    );
    let worker_state = machines
        .iter()
        .find(|(id, _)| id == WORKER_NODE_ID)
        .and_then(|(_, state)| *state);
    assert_ne!(worker_state, Some(NodeObservationState::Connected));
    assert_eq!(
        listed_deployments(manager_address).await,
        [deployment_id.as_str()]
    );
    assert_eq!(listed_routes(manager_address).await, [MANAGER_ROUTE]);
    assert_eq!(
        chat_status(&harness, MANAGER_ROUTE).await,
        reqwest::StatusCode::OK
    );
}

/// Returns the sorted node ids the manager's `site.yaml` declares.
fn declared_node_ids(harness: &TwoProcessObservationHarness) -> Vec<String> {
    let mut ids = harness
        .manager_site()
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}
