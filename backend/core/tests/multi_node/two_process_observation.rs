//! Exercises worker observation across separate manager and generated-folder backend processes.
//!
//! The harness uses real mutual TLS, process restart, and process suspension so manager projections
//! prove worker liveness and incarnation fencing without sharing authoritative site files.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::Duration,
};

use dotcore_backend::testing::{LIVENESS_TTL, ManualClock, NodeLivenessAges};
use dotcore_proto::{
    dotcore::{
        observation::v1::{ObservationUnavailable, RuntimeObservation, runtime_observation},
        site::private::v1::{
            DesiredViewSectionKind, NodeControlOpen, NodeHardwareObservation, NodeObservation,
            NodeObservationSnapshot, SupportedDesiredViewSection, node_hardware_observation,
            stream_node_control_request, stream_node_control_response,
        },
        site::v1::{
            ListHardwareSnapshotsRequest, ListMachinesRequest, Machine, NodeObservationState,
            current_hardware_snapshot_availability, hardware_refresh_state,
            site_service_client::SiteServiceClient,
        },
    },
    google::protobuf::{Empty, Timestamp},
};
use dotcore_site::node::folder::ValidatedNodeFolder;
use support::{ProcessCommand, node_control::ControlStream, node_liveness, process_protocol};

use crate::support;

const PROJECTION_DEADLINE: Duration = Duration::from_secs(15);
/// Refusal the manager returns to a node control stream open from a process whose incarnation
/// another one replaced.
const FENCED_OPEN_REFUSAL: &str = "node process incarnation was replaced";
/// Deadline for a liveness expiry: the window plus a margin for the last contact and sampling.
const LIVENESS_EXPIRY_DEADLINE: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Deadline for a worker to rejoin a restarted manager after its path went silent: one stalled
/// report's TLS handshake bound, the next stream open and view, and process startup under load.
const SILENT_PATH_RECOVERY_DEADLINE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn each_node_reports_its_own_declared_machine() {
    let Some(harness) =
        support::TwoProcessObservationHarness::start_with_manager_machine("mac-64g")
    else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    let idle = |machine: &str| {
        dotcore_node_hardware::MachineDescription::named(machine)
            .expect("named machine")
            .render(&[], std::time::UNIX_EPOCH)
            .expect("idle machine")
    };

    support::wait_for_listed_hardware(manager_address, "fixture-node", idle("mac-64g")).await;
    support::wait_for_listed_hardware(
        manager_address,
        support::WORKER_NODE_ID,
        support::simulated_machine(
            support::WorkerModelTarget::SimulatedLinux
                .simulated_hardware()
                .expect("simulated worker hardware"),
        )
        .render(&[], std::time::UNIX_EPOCH)
        .expect("idle machine"),
    )
    .await;
}

#[tokio::test]
async fn two_process_observation_fences_restarts_and_projects_connection_loss() {
    let Some(mut harness) = support::TwoProcessObservationHarness::start_with_controls(
        support::ProcessTestControls::manual_clock(),
        support::ProcessTestControls::default(),
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    let manager_root = harness.manager().site_root().to_owned();
    let worker_root = harness.worker().site_root().to_owned();
    let manager_site_before = fs::read(manager_root.join("site.yaml")).expect("manager site");
    let folder = harness.node_folder();

    assert_ne!(manager_root, worker_root);
    assert!(manager_root.join("site.yaml").is_file());
    assert!(manager_root.join("node.yaml").is_file());
    assert!(!worker_root.join("site.yaml").exists());
    assert!(!worker_root.join("node.yaml").exists());
    assert!(worker_root.join("worker.yaml").is_file());
    assert!(harness.manager().addresses().private.is_some());
    assert!(harness.worker().addresses().private.is_some());

    let mut site = SiteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager site API connection");
    assert_eq!(harness.worker().addresses().management, None);

    let initial = wait_for_projection(&mut site, |machines| {
        manager_machine_is_available(machines)
            && worker_machine(machines).is_some_and(|worker| {
                !worker.site_manager
                    && connection_state(worker) == NodeObservationState::Connected
                    && machine_hardware_is_available(worker)
                    && node_runtime_targets_match(worker, &folder)
            })
    })
    .await;
    assert_eq!(
        machine_ids(&initial),
        BTreeSet::from(["fixture-node", support::WORKER_NODE_ID])
    );
    let worker_hardware = site
        .list_hardware_snapshots(ListHardwareSnapshotsRequest {})
        .await
        .expect("worker hardware detail")
        .into_inner()
        .machines
        .into_iter()
        .find(|machine| machine.machine_id == support::WORKER_NODE_ID)
        .expect("worker hardware entry");
    assert!(worker_hardware.snapshot.is_some());
    assert!(matches!(
        worker_hardware
            .refresh
            .expect("worker hardware refresh")
            .outcome,
        Some(hardware_refresh_state::Outcome::Succeeded(_))
    ));
    assert_eq!(
        worker_hardware
            .observation_freshness
            .expect("worker hardware freshness")
            .state,
        NodeObservationState::Connected as i32
    );

    // A suspended worker stops all contact, so it goes from Connected straight to Disconnected
    // once the liveness window passes, keeping its last observation. The baseline is read once the
    // manager has seen the worker silent, so no report sent before the pause can still land.
    harness.worker_mut().pause();
    assert!(harness.worker_mut().is_running());
    let initial_received_at = receipt_after_silence(&mut harness, &mut site).await;
    let silent = wait_for_disconnection(&mut site).await;
    assert_eq!(worker_received_at(&silent), initial_received_at);
    assert!(harness.worker_mut().is_running());

    let replacement_incarnation = "a".repeat(32);
    let mut replacement_stream = ControlStream::open(
        harness.worker_control_channel().await,
        stream_open(&folder, &replacement_incarnation),
    )
    .await;
    replacement_stream.opened().await;
    replacement_stream.send(stream_node_control_request::Message::Observation(
        observation_report(&folder, replacement_incarnation.clone(), 1),
    ));
    let Ok(stream_node_control_response::Message::ObservationAccepted(replacement)) =
        replacement_stream.next().await
    else {
        panic!("replace silent worker incarnation")
    };
    assert_eq!(replacement.accepted_revision, 1);
    let replacement_received_at = replacement.received_at.expect("replacement receipt time");

    // The resumed original process is fenced, so its stream opens never renew the replacement,
    // whose stream stays open but sends nothing more and disconnects one liveness window later. Two refusals counted after the
    // first half of the window: the first may have been handled before it, but opens are tried
    // one at a time, so the second was handled after it, and the second half then requires the
    // replacement to have stayed silent through both.
    harness.worker_mut().resume();
    assert!(harness.worker_mut().is_running());
    let half_lease = LIVENESS_TTL / 2;
    harness.advance_past_silent_worker(half_lease).await;
    let refused = support::refused_stream_opens(&worker_root, FENCED_OPEN_REFUSAL);
    support::wait_for_refused_stream_opens(&worker_root, FENCED_OPEN_REFUSAL, refused + 2).await;
    harness
        .advance_past_silent_worker(half_lease + Duration::from_millis(1))
        .await;
    let disconnected = wait_for_disconnection(&mut site).await;
    assert_eq!(worker_received_at(&disconnected), replacement_received_at);
    drop(replacement_stream);
    assert!(harness.worker_mut().is_running());
    assert_eq!(
        machine_ids(&disconnected),
        BTreeSet::from(["fixture-node", support::WORKER_NODE_ID])
    );
    assert_eq!(
        fs::read(manager_root.join("site.yaml")).expect("manager site after connection loss"),
        manager_site_before
    );

    harness.worker_mut().restart();
    wait_for_projection(&mut site, |machines| {
        worker_machine(machines).is_some_and(|worker| {
            connection_state(worker) == NodeObservationState::Connected
                && worker_received_at(machines) != replacement_received_at
        })
    })
    .await;
    assert!(harness.worker_mut().is_running());

    let old_update = fenced_open(&harness, &folder, &replacement_incarnation)
        .await
        .expect_err("replaced incarnation must remain fenced");
    assert_eq!(old_update.code(), tonic::Code::FailedPrecondition);

    harness.worker_mut().pause();
    assert!(harness.worker_mut().is_running());
    let restarted_received_at = receipt_after_silence(&mut harness, &mut site).await;
    harness
        .advance_past_silent_worker(LIVENESS_TTL / 2 + Duration::from_millis(1))
        .await;
    let final_disconnected = wait_for_disconnection(&mut site).await;
    assert_eq!(
        worker_received_at(&final_disconnected),
        restarted_received_at
    );
    assert!(harness.worker_mut().is_running());
    let old_update = fenced_open(&harness, &folder, &replacement_incarnation)
        .await
        .expect_err("fenced update cannot revive disconnected worker");
    assert_eq!(old_update.code(), tonic::Code::FailedPrecondition);
    let after_fenced_update = list_machines(&mut site).await;
    assert_eq!(
        connection_state(worker_machine(&after_fenced_update).expect("worker machine")),
        NodeObservationState::Disconnected
    );
    assert_eq!(
        worker_received_at(&after_fenced_update),
        restarted_received_at
    );
    assert_eq!(
        machine_ids(&after_fenced_update),
        BTreeSet::from(["fixture-node", support::WORKER_NODE_ID])
    );
    assert_eq!(
        fs::read(manager_root.join("site.yaml")).expect("manager site after fencing"),
        manager_site_before
    );
    harness.worker_mut().resume();
}

#[tokio::test]
async fn a_worker_rejoins_a_restarted_manager_after_its_path_went_silent() {
    let Some(mut harness) = support::TwoProcessObservationHarness::start_split(
        support::ProcessTestControls::default(),
        support::ProcessTestControls::default(),
        &[],
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    let worker_root = harness.worker().site_root().to_owned();
    assert!(
        wait_until(PROJECTION_DEADLINE, || support::installed_views(
            &worker_root
        ) >= 1)
        .await,
        "the worker installs a first view"
    );
    let installed_before = support::installed_views(&worker_root);

    // The manager host vanishes without closing connections: open ones go quiet, and new ones
    // are accepted but never answered, as when the host disappears after accepting them.
    harness.manager_path().set_silent(true);
    assert!(
        wait_until(PROJECTION_DEADLINE, || harness
            .manager_path()
            .held_connections()
            >= 1)
        .await,
        "a worker connection is held by the silent path"
    );
    harness.manager_mut().restart();
    harness.manager_path().set_silent(false);

    let rejoined = wait_until(SILENT_PATH_RECOVERY_DEADLINE, || {
        support::installed_views(&worker_root) > installed_before
    })
    .await;
    assert!(
        rejoined,
        "the same worker process installs a view from the restarted manager\n\
         worker log:\n{}\nmanager log:\n{}",
        support::retained_log_text(&worker_root),
        support::retained_log_text(harness.manager().site_root())
    );
    let restarted_manager = harness
        .manager()
        .addresses()
        .management
        .expect("restarted management listener");
    support::wait_for_worker_connection(restarted_manager).await;
    assert!(
        harness.worker_mut().is_running(),
        "the worker rejoined without a restart"
    );
}

#[test]
fn renewed_live_nodes_and_silent_expiring_nodes_allow_an_advance() {
    let ages = BTreeMap::from([
        node_ages("manager", 500, Some(30_000)),
        node_ages("worker-one", 4_000, Some(4_000)),
        node_ages("worker-two", 1_900, None),
    ]);

    assert_eq!(
        node_liveness::renewal_problem(&ages, &["worker-one".to_owned()], millis(2_000)),
        None,
        "a streamed status may be old while its node renews contact"
    );
}

#[test]
fn a_live_node_must_renew_contact_after_the_request() {
    let expiring = ["worker-one".to_owned()];
    let silent = node_ages("worker-one", 5_000, None);
    for (live, elapsed) in [
        (node_ages("manager", 600, Some(200)), 500),
        (node_ages("manager", 2_100, Some(200)), 3_000),
    ] {
        assert_eq!(
            node_liveness::renewal_problem(
                &BTreeMap::from([live, silent.clone()]),
                &expiring,
                millis(elapsed)
            ),
            Some("live node manager has not renewed its contact yet".to_owned())
        );
    }
}

#[test]
fn an_expiring_node_must_be_known_and_silent() {
    let manager = node_ages("manager", 100, Some(100));
    assert_eq!(
        node_liveness::renewal_problem(
            &BTreeMap::from([manager.clone()]),
            &["worker-one".to_owned()],
            millis(500)
        ),
        Some("expiring node worker-one has no current incarnation".to_owned())
    );
    assert_eq!(
        node_liveness::renewal_problem(
            &BTreeMap::from([manager, node_ages("worker-one", 3_900, None)]),
            &["worker-one".to_owned()],
            millis(500)
        ),
        Some("expiring node worker-one is not silent yet".to_owned())
    );
}

#[test]
fn an_advance_refuses_a_step_that_could_expire_a_renewing_node() {
    let clock = ManualClock::new();

    assert_eq!(
        node_liveness::advance(
            &clock,
            BTreeMap::new,
            Duration::from_secs(9),
            &[],
            Duration::ZERO
        ),
        Err("advancing the manager clock by 9s could expire a node that keeps renewing".to_owned())
    );
    assert_eq!(clock.offset(), Duration::ZERO);
}

#[test]
fn an_advance_with_renewed_nodes_moves_the_clock() {
    let clock = ManualClock::new();

    node_liveness::advance(
        &clock,
        BTreeMap::new,
        Duration::from_secs(5),
        &[],
        Duration::ZERO,
    )
    .expect("no node needs to renew");

    assert_eq!(clock.offset(), Duration::from_secs(5));
}

#[test]
fn a_failed_advance_reports_the_last_observed_ages() {
    let clock = ManualClock::new();
    let ages = || BTreeMap::from([node_ages("worker-one", 1_900, None)]);

    assert_eq!(
        node_liveness::advance(
            &clock,
            ages,
            Duration::from_secs(5),
            &["worker-one".to_owned()],
            Duration::ZERO,
        ),
        Err(format!(
            "expiring node worker-one is not silent yet; node liveness ages {:?}",
            ages()
        ))
    );
    assert_eq!(clock.offset(), Duration::ZERO);
}

#[test]
fn commands_without_their_controls_are_refused() {
    assert_eq!(
        process_protocol::execute(ProcessCommand::ReleaseReadinessGate, None, None),
        Err("model transition gates are not enabled".to_owned())
    );
    assert_eq!(
        process_protocol::execute(
            ProcessCommand::AdvanceClock {
                by: Duration::from_secs(5),
                expiring: Vec::new(),
            },
            None,
            None,
        ),
        Err("the manager does not run on a manual clock".to_owned())
    );
}

/// Returns the worker's receipt time once the manager has seen the paused worker silent, after
/// which no report sent before the pause can still be accepted. Consumes half the liveness window
/// on the manager's clock.
async fn receipt_after_silence(
    harness: &mut support::TwoProcessObservationHarness,
    site: &mut SiteServiceClient<tonic::transport::Channel>,
) -> Timestamp {
    harness.advance_past_silent_worker(LIVENESS_TTL / 2).await;
    worker_received_at(&list_machines(site).await)
}

/// Samples the worker until it is Disconnected, failing if it is ever projected Stale on the way:
/// a silent worker loses liveness without an intermediate step.
async fn wait_for_disconnection(
    site: &mut SiteServiceClient<tonic::transport::Channel>,
) -> Vec<Machine> {
    assert!(LIVENESS_EXPIRY_DEADLINE > LIVENESS_TTL);
    let stale_seen = std::cell::Cell::new(false);
    let machines = wait_for_projection_within(site, LIVENESS_EXPIRY_DEADLINE, |machines| {
        let state = worker_machine(machines).map(connection_state);
        stale_seen.set(stale_seen.get() || state == Some(NodeObservationState::Stale));
        state == Some(NodeObservationState::Disconnected)
    })
    .await;
    assert!(
        !stale_seen.get(),
        "a silent worker disconnects without a Stale step"
    );
    machines
}

async fn wait_for_projection(
    site: &mut SiteServiceClient<tonic::transport::Channel>,
    predicate: impl Fn(&[Machine]) -> bool,
) -> Vec<Machine> {
    wait_for_projection_within(site, PROJECTION_DEADLINE, predicate).await
}

async fn wait_for_projection_within(
    site: &mut SiteServiceClient<tonic::transport::Channel>,
    deadline: Duration,
    predicate: impl Fn(&[Machine]) -> bool,
) -> Vec<Machine> {
    tokio::time::timeout(deadline, async {
        loop {
            let machines = list_machines(site).await;
            if predicate(&machines) {
                return machines;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("Site projection deadline")
}

/// Polls `condition` until it holds or `deadline` passes, returning whether it held.
async fn wait_until(deadline: Duration, condition: impl Fn() -> bool) -> bool {
    tokio::time::timeout(deadline, async {
        while !condition() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .is_ok()
}

async fn list_machines(site: &mut SiteServiceClient<tonic::transport::Channel>) -> Vec<Machine> {
    site.list_machines(ListMachinesRequest {})
        .await
        .expect("manager machine inventory")
        .into_inner()
        .machines
}

fn manager_machine_is_available(machines: &[Machine]) -> bool {
    machines.iter().any(|machine| {
        machine.machine_id == "fixture-node"
            && machine.site_manager
            && connection_state(machine) == NodeObservationState::Connected
            && machine_hardware_is_available(machine)
    })
}

fn machine_hardware_is_available(machine: &Machine) -> bool {
    matches!(
        machine
            .current_hardware_snapshot_availability
            .as_ref()
            .and_then(|availability| availability.availability.as_ref()),
        Some(current_hardware_snapshot_availability::Availability::Available(_))
    )
}

fn node_runtime_targets_match(worker: &Machine, folder: &ValidatedNodeFolder) -> bool {
    worker.runtimes.len() == folder.observation_configuration().execution_targets.len()
        && worker
            .runtimes
            .iter()
            .zip(&folder.observation_configuration().execution_targets)
            .all(|(runtime, target)| {
                runtime.execution_target_id == target.execution_target_id
                    && runtime.kind == target.kind
                    && matches!(
                        runtime.result,
                        Some(runtime_observation::Result::Available(_))
                    )
            })
}

fn worker_machine(machines: &[Machine]) -> Option<&Machine> {
    machines
        .iter()
        .find(|machine| machine.machine_id == support::WORKER_NODE_ID)
}

fn connection_state(machine: &Machine) -> NodeObservationState {
    NodeObservationState::try_from(
        machine
            .observation_freshness
            .as_ref()
            .expect("machine observation freshness")
            .state,
    )
    .expect("known observation state")
}

fn worker_received_at(machines: &[Machine]) -> Timestamp {
    worker_machine(machines)
        .expect("worker machine")
        .observation_freshness
        .as_ref()
        .expect("worker observation freshness")
        .received_at
        .expect("worker observation receipt time")
}

fn machine_ids(machines: &[Machine]) -> BTreeSet<&str> {
    machines
        .iter()
        .map(|machine| machine.machine_id.as_str())
        .collect()
}

/// Opens a node control stream of `incarnation_id` for the worker and returns the manager's first
/// answer, or the status that ended the stream.
async fn fenced_open(
    harness: &support::TwoProcessObservationHarness,
    folder: &ValidatedNodeFolder,
    incarnation_id: &str,
) -> Result<stream_node_control_response::Message, tonic::Status> {
    ControlStream::open(
        harness.worker_control_channel().await,
        stream_open(folder, incarnation_id),
    )
    .await
    .next()
    .await
}

fn stream_open(folder: &ValidatedNodeFolder, incarnation_id: &str) -> NodeControlOpen {
    NodeControlOpen {
        site_id: folder.site_id().to_owned(),
        node_id: folder.worker_node_id().to_owned(),
        node_incarnation_id: incarnation_id.to_owned(),
        supported_sections: vec![SupportedDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            minimum_schema_version: 1,
            maximum_schema_version: 1,
        }],
    }
}

fn observation_report(
    folder: &ValidatedNodeFolder,
    incarnation_id: String,
    revision: u64,
) -> NodeObservation {
    NodeObservation {
        site_id: folder.site_id().to_owned(),
        node_id: folder.worker_node_id().to_owned(),
        incarnation_id,
        revision,
        snapshot: Some(NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Unavailable(
                    ObservationUnavailable {
                        code: "TEST_COLLECTION_PAUSED".to_owned(),
                        message: "Harness-controlled observation replacement.".to_owned(),
                    },
                )),
            }),
            runtimes: folder
                .observation_configuration()
                .execution_targets
                .iter()
                .map(|target| RuntimeObservation {
                    execution_target_id: target.execution_target_id.clone(),
                    kind: target.kind,
                    result: Some(runtime_observation::Result::Available(Empty {})),
                })
                .collect(),
        }),
        observation_configuration_sha256: folder
            .observation_configuration_sha256()
            .as_str()
            .to_owned(),
        private_port: None,
    }
}

fn node_ages(node_id: &str, contact: u64, status: Option<u64>) -> (String, NodeLivenessAges) {
    (
        node_id.to_owned(),
        NodeLivenessAges {
            contact: millis(contact),
            status: status.map(millis),
        },
    )
}

fn millis(millis: u64) -> Duration {
    Duration::from_millis(millis)
}
