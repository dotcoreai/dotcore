//! Proves the manager reads and follows the retained backend logs of every machine: its own in
//! process and a separate worker process over the private channel, each tagged by machine.
//!
//! New log lines are triggered through the diagnostic API itself: a refused worker read is logged
//! by the worker, and an unreachable worker is logged by the manager. Every wait polls an
//! observed condition within a bounded deadline.

use std::{net::SocketAddr, time::Duration};

use dotcore_proto::dotcore::diagnostic::v1::{
    DiagnosticHistoryContinuation, DiagnosticSourceState, FollowBackendDiagnosticsRequest,
    FollowBackendDiagnosticsResponse, MachineDiagnosticPosition, ReadBackendDiagnosticsRequest,
    ReadBackendDiagnosticsResponse, diagnostic_service_client::DiagnosticServiceClient,
    follow_backend_diagnostics_response::Event,
};
use tonic::{Streaming, transport::Channel};

use crate::support;

const DEADLINE: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const WORKER: &str = support::WORKER_NODE_ID;
const WORKER_REFUSAL: &str = "Node refused a manager diagnostic request";
/// What the manager logs when it cannot dial the stopped worker, or when the call to it fails.
const MANAGER_UNREACHABLE: &[&str] = &[
    "Manager could not reach a node for its diagnostics",
    "Node diagnostics are unavailable",
];

#[tokio::test]
async fn manager_reads_and_follows_backend_logs_of_every_machine() {
    let Some(mut harness) = support::TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    let machines = support::machine_ids(manager_address).await;
    let manager = machines
        .iter()
        .find(|machine| machine.as_str() != WORKER)
        .expect("manager machine")
        .clone();
    let mut diagnostics = client(manager_address).await;

    // The newest page merges both machines' startup logs.
    let page = poll_newest(&mut diagnostics, |page| {
        has_events_of(page, &manager) && has_events_of(page, WORKER)
    })
    .await;
    assert_eq!(
        page.machines
            .iter()
            .map(|machine| (machine.machine_id.as_str(), machine.state))
            .collect::<Vec<_>>(),
        machines
            .iter()
            .map(|machine| (machine.as_str(), DiagnosticSourceState::Available as i32))
            .collect::<Vec<_>>()
    );
    assert!(
        page.events
            .windows(2)
            .all(|pair| timestamp(&pair[0]) <= timestamp(&pair[1])),
        "the merged page is chronological"
    );

    let mut follow = diagnostics
        .follow_backend_diagnostics(FollowBackendDiagnosticsRequest::default())
        .await
        .expect("follow every machine")
        .into_inner();
    let mut available = Vec::new();
    while available.len() < 2 {
        if let Event::SourceEvent(state) = next(&mut follow).await
            && state.state == DiagnosticSourceState::Available as i32
        {
            available.push(state.machine_id);
        }
    }
    available.sort();
    assert_eq!(available, machines);

    // A refused read makes the worker log a line, which the follow carries tagged by the worker.
    let refused = read(
        &mut diagnostics,
        Some(DiagnosticHistoryContinuation {
            positions: vec![MachineDiagnosticPosition {
                machine_id: WORKER.to_owned(),
                position: b"not-a-position".to_vec(),
            }],
        }),
    )
    .await;
    assert_eq!(refused.machines.len(), 1);
    assert_eq!(
        refused.machines[0].state,
        DiagnosticSourceState::Unavailable as i32
    );
    wait_for_message(&mut follow, WORKER, &[WORKER_REFUSAL]).await;

    // A stopped worker is reported unavailable once, while the manager's lines keep arriving.
    harness.worker_mut().stop();
    wait_for_state(&mut follow, WORKER, DiagnosticSourceState::Unavailable).await;
    let unreachable = read(&mut diagnostics, None).await;
    assert!(
        unreachable
            .machines
            .iter()
            .any(|machine| machine.machine_id == WORKER
                && machine.state == DiagnosticSourceState::Unavailable as i32)
    );
    wait_for_message(&mut follow, &manager, MANAGER_UNREACHABLE).await;
}

async fn client(manager_address: SocketAddr) -> DiagnosticServiceClient<Channel> {
    DiagnosticServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager diagnostic API connection")
}

async fn read(
    diagnostics: &mut DiagnosticServiceClient<Channel>,
    before: Option<DiagnosticHistoryContinuation>,
) -> ReadBackendDiagnosticsResponse {
    diagnostics
        .read_backend_diagnostics(ReadBackendDiagnosticsRequest {
            page_size: 200,
            before,
        })
        .await
        .expect("site diagnostic page")
        .into_inner()
}

/// Reads the newest page until `done` holds, reporting the last page on timeout.
async fn poll_newest(
    diagnostics: &mut DiagnosticServiceClient<Channel>,
    done: impl Fn(&ReadBackendDiagnosticsResponse) -> bool,
) -> ReadBackendDiagnosticsResponse {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let page = read(diagnostics, None).await;
        if done(&page) {
            return page;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "site diagnostics never satisfied the condition; last page machines: {:?}",
            page.machines
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn next(follow: &mut Streaming<FollowBackendDiagnosticsResponse>) -> Event {
    tokio::time::timeout(DEADLINE, follow.message())
        .await
        .expect("follow event in time")
        .expect("follow without error")
        .expect("follow still open")
        .event
        .expect("follow event payload")
}

async fn wait_for_message(
    follow: &mut Streaming<FollowBackendDiagnosticsResponse>,
    machine_id: &str,
    messages: &[&str],
) {
    loop {
        if let Event::Diagnostic(event) = next(follow).await
            && event.machine_id == machine_id
            && event
                .event
                .as_ref()
                .is_some_and(|event| messages.contains(&event.message.as_str()))
        {
            return;
        }
    }
}

async fn wait_for_state(
    follow: &mut Streaming<FollowBackendDiagnosticsResponse>,
    machine_id: &str,
    expected: DiagnosticSourceState,
) {
    loop {
        if let Event::SourceEvent(state) = next(follow).await
            && state.machine_id == machine_id
            && state.state == expected as i32
        {
            return;
        }
    }
}

fn has_events_of(page: &ReadBackendDiagnosticsResponse, machine_id: &str) -> bool {
    page.events
        .iter()
        .any(|event| event.machine_id == machine_id)
}

fn timestamp(event: &dotcore_proto::dotcore::diagnostic::v1::MachineDiagnosticEvent) -> (i64, i32) {
    event
        .event
        .as_ref()
        .and_then(|event| event.timestamp.as_ref())
        .map_or((0, 0), |timestamp| (timestamp.seconds, timestamp.nanos))
}
