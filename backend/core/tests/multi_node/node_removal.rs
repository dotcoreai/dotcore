//! Proves a manager restarts cleanly after its worker node is removed offline, that the removed
//! worker is told it is no longer a member and stops, and that a node of the same name joins again and serves a
//! model. A second scenario removes only the worker's pin and proves the manager still starts
//! and serves its own deployments. Two more remove a worker from the running manager: the worker
//! is told and stops, and one with deployments is removed only once disconnected and forgotten.
//!
//! The scenarios use two real processes with generated worker custody and mutual TLS, and the
//! simulated model runtime, so they need a host that observes an NVIDIA driver.

#![cfg(target_os = "linux")]

use std::fs;

use dotcore::model::engine::declaration_validator;
use dotcore_proto::dotcore::site::v1::{
    MachineRemovalRefusal, MachineRemovalRefusalReason, NodeObservationState, RemoveMachineRequest,
    remove_machine_response, site_membership_service_client::SiteMembershipServiceClient,
};
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

    // The old worker folder starts, is told it is no longer a member, and stops.
    let worker_root = harness.worker().site_root().to_owned();
    harness.worker_mut().restart();
    let status = harness.worker_mut().wait_for_exit().await;
    assert!(!status.success(), "{status:?}");
    assert!(support::logged_removal(&worker_root));
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

#[tokio::test]
async fn a_worker_removed_from_the_running_site_is_told_and_stops() {
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
    let worker_root = harness.worker().site_root().to_owned();

    let removed = remove_machine(manager_address, WORKER_NODE_ID, false).await;

    let Some(remove_machine_response::Outcome::Removed(plan)) = removed else {
        panic!("expected the worker removed, got {removed:?}");
    };
    assert_eq!(plan.node_id, WORKER_NODE_ID);
    let status = harness.worker_mut().wait_for_exit().await;
    assert!(!status.success(), "{status:?}");
    assert!(support::logged_removal(&worker_root));
    assert_eq!(machine_ids(manager_address).await, [MANAGER_NODE_ID]);
    assert_eq!(declared_node_ids(&harness), [MANAGER_NODE_ID]);
    assert!(!worker_pin(&manager_root).exists());
    assert!(matches!(
        remove_machine(manager_address, WORKER_NODE_ID, false).await,
        Some(remove_machine_response::Outcome::AlreadyRemoved(_))
    ));
}

#[tokio::test]
async fn a_worker_with_deployments_is_removed_only_once_disconnected_and_forgotten() {
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
    let worker_root = harness.worker().site_root().to_owned();
    let deployment_id = place_model(
        manager_address,
        WORKER_NODE_ID,
        support::WORKER_MODEL_TARGET_ID,
        WORKER_ROUTE,
    )
    .await;
    wait_for_ready(manager_address, &deployment_id).await;

    // Placed deployments refuse the removal, and a connected worker keeps them.
    let refused = refusal(remove_machine(manager_address, WORKER_NODE_ID, false).await);
    assert_eq!(
        refused.reason(),
        MachineRemovalRefusalReason::DeploymentsPlaced
    );
    assert_eq!(
        refused
            .deployments
            .iter()
            .map(|deployment| deployment.deployment_id.as_str())
            .collect::<Vec<_>>(),
        [deployment_id.as_str()]
    );
    let refused = refusal(remove_machine(manager_address, WORKER_NODE_ID, true).await);
    assert_eq!(refused.reason(), MachineRemovalRefusalReason::Connected);
    assert_eq!(
        listed_deployments(manager_address).await,
        [deployment_id.as_str()]
    );

    // Once the worker is gone, its deployment and route are forgotten with it.
    harness.worker_mut().stop();
    wait_for_disconnected_worker(manager_address).await;
    let removed = remove_machine(manager_address, WORKER_NODE_ID, true).await;
    let Some(remove_machine_response::Outcome::Removed(plan)) = removed else {
        panic!("expected the worker removed, got {removed:?}");
    };
    assert_eq!(
        plan.forgotten_deployments
            .iter()
            .map(|deployment| deployment.deployment_id.as_str())
            .collect::<Vec<_>>(),
        [deployment_id.as_str()]
    );
    assert_eq!(
        plan.deleted_routes
            .iter()
            .map(|route| route.name.as_str())
            .collect::<Vec<_>>(),
        [WORKER_ROUTE]
    );
    assert_eq!(
        listed_deployments(manager_address).await,
        Vec::<String>::new()
    );
    assert_eq!(listed_routes(manager_address).await, Vec::<String>::new());

    // The worker that comes back is told it was removed and stops.
    harness.worker_mut().restart();
    let status = harness.worker_mut().wait_for_exit().await;
    assert!(!status.success(), "{status:?}");
    assert!(support::logged_removal(&worker_root));
    assert_eq!(machine_ids(manager_address).await, [MANAGER_NODE_ID]);
}

/// Removes `node_id` through the manager's membership API and returns the outcome.
async fn remove_machine(
    manager_address: std::net::SocketAddr,
    node_id: &str,
    forget_deployments: bool,
) -> Option<remove_machine_response::Outcome> {
    SiteMembershipServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager membership API connection")
        .remove_machine(RemoveMachineRequest {
            node_id: node_id.to_owned(),
            forget_deployments,
            validate_only: false,
        })
        .await
        .expect("manager answers the removal")
        .into_inner()
        .outcome
}

fn refusal(outcome: Option<remove_machine_response::Outcome>) -> MachineRemovalRefusal {
    match outcome {
        Some(remove_machine_response::Outcome::Refused(refusal)) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Waits until the manager no longer sees the worker connected.
async fn wait_for_disconnected_worker(manager_address: std::net::SocketAddr) {
    const DISCONNECT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

    let mut last = Vec::new();
    let disconnected = tokio::time::timeout(DISCONNECT_DEADLINE, async {
        loop {
            last = machine_states(manager_address).await;
            if last.iter().any(|(id, state)| {
                id == WORKER_NODE_ID && *state != Some(NodeObservationState::Connected)
            }) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        disconnected.is_ok(),
        "the worker stayed connected; machines: {last:?}"
    );
}

fn worker_pin(manager_root: &std::path::Path) -> std::path::PathBuf {
    manager_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY)
        .join(format!("{WORKER_NODE_ID}.sha256"))
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
