use dotcore_domain::timing::Timing;
use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    num::NonZeroU16,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use dotcore_domain::test_support::ports::{ReservedPort, reserve_port};
use dotcore_proto::dotcore::{
    deployment::{
        private::v1::{
            DeploymentLogFollowOpened, DeploymentLogRejectionReason, DeploymentLogResumePosition,
            DeploymentLogSource, DeploymentLogSourceEnded, DeploymentLogSourceOpening,
            DeploymentLogTarget, DeploymentLogUnavailableReason, DeploymentOwnerKind,
            FollowDeploymentLogsRequest, FollowDeploymentLogsResponse, ReadDeploymentLogsRequest,
            ReadDeploymentLogsResponse, deployment_log_service_server::DeploymentLogService as _,
            deployment_log_source_history, follow_deployment_logs_response,
            read_deployment_logs_response,
        },
        v1::ExactPlacement,
    },
    model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
    site::v1::ExecutionTarget,
};
use dotcore_runtime::{
    MAX_RUNTIME_LOG_SOURCES, RuntimeBackendError, RuntimeLogEntry, RuntimeLogEvent,
    RuntimeLogHistory, RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceHistory,
    RuntimeLogSourceStream, RuntimeLogStream, RuntimeLogStreamKind,
};
use dotcore_site::{
    private_transport::{AuthenticatedMachineIdentity, PrivateNodeRole, PrivateServiceContext},
    view::fencing::{
        AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
    },
    worker_channels::{WorkerChannelPurpose, WorkerChannels, WorkerConnection},
};
use futures_util::StreamExt;
use prost::Message;
use tokio_util::sync::CancellationToken;
use tonic::Request;

use super::{
    DeploymentLogOwner, DeploymentLogOwnerFuture, DeploymentLogOwners, DeploymentLogReader,
    DeploymentLogService, NodeLogRequest, NodeLogSelection, OwnerLogSources, RESPONSE_BUDGET_BYTES,
    WorkloadLifecycle,
    client::{StillPresent, follow_from, read_from},
};
use crate::{
    management_logs::{
        DeploymentLogDiagnosticContext, DeploymentLogError, DeploymentLogFollowEvent,
        DeploymentLogFollowQuery, DeploymentLogReadQuery, MAX_LOG_ENTRY_CONTENT_BYTES,
        ManagementLogAdmission, ProtectedLogValues, follow_response, follow_sources,
    },
    server::private::PrivateServiceAssembly,
    test_support::private_tls::TestAuthority,
};

const SITE_ID: &str = "site-one";
const WORKER_NODE_ID: &str = "worker-one";
const MANAGER_NODE_ID: &str = "manager-one";
const DEPLOYMENT: &str = "deployment-a";
const PROMPT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn only_the_manager_is_admitted() {
    let worker = Worker::new(ScriptedOwner::reading(Ok(Vec::new())));

    let unauthenticated = worker
        .service
        .read_deployment_logs(authenticated(read_request(target()), None))
        .await
        .expect_err("an unauthenticated caller is refused");
    let from_worker = worker
        .service
        .read_deployment_logs(authenticated(
            read_request(target()),
            Some(PrivateNodeRole::Worker),
        ))
        .await
        .expect_err("another worker is refused");

    assert_eq!(unauthenticated.code(), tonic::Code::Unauthenticated);
    assert_eq!(from_worker.code(), tonic::Code::PermissionDenied);
    assert!(worker.owner.calls().is_empty());
}

#[tokio::test]
async fn a_stale_identity_or_malformed_target_is_rejected_before_the_owner() {
    let worker = Worker::new(ScriptedOwner::reading(Ok(Vec::new())));
    let stale_incarnation = DeploymentLogTarget {
        node_incarnation_id: "2".repeat(32),
        ..target()
    };
    let stale_session = DeploymentLogTarget {
        manager_session_id: "b".repeat(32),
        ..target()
    };
    let unknown_owner = DeploymentLogTarget {
        owner_kind: DeploymentOwnerKind::Unspecified as i32,
        ..target()
    };

    let rejections = [
        worker.read(read_request(stale_incarnation)).await,
        worker.read(read_request(stale_session)).await,
        worker.read(read_request(unknown_owner)).await,
        worker
            .read(ReadDeploymentLogsRequest {
                target: None,
                ..read_request(target())
            })
            .await,
        worker
            .read(ReadDeploymentLogsRequest {
                maximum_entries: 201,
                ..read_request(target())
            })
            .await,
    ]
    .map(|response| rejection_reason(&response));

    assert_eq!(
        rejections,
        [
            Some(DeploymentLogRejectionReason::NodeIncarnationMismatch),
            Some(DeploymentLogRejectionReason::ManagerSessionMismatch),
            Some(DeploymentLogRejectionReason::InvalidRequest),
            Some(DeploymentLogRejectionReason::InvalidRequest),
            Some(DeploymentLogRejectionReason::InvalidRequest),
        ]
    );
    assert!(worker.owner.calls().is_empty());
}

#[tokio::test]
async fn owner_refusals_become_typed_rejections() {
    let not_found = Worker::new(ScriptedOwner::reading(Err(DeploymentLogError::NotFound)));
    let invalid_source = Worker::new(ScriptedOwner::reading(Err(
        DeploymentLogError::InvalidSource,
    )));

    assert_eq!(
        rejection_reason(&not_found.read(read_request(target())).await),
        Some(DeploymentLogRejectionReason::DeploymentNotFound)
    );
    assert_eq!(
        rejection_reason(&invalid_source.read(read_request(target())).await),
        Some(DeploymentLogRejectionReason::InvalidSource)
    );
    assert_eq!(not_found.owner.calls(), [DEPLOYMENT]);
}

#[tokio::test]
async fn application_deployments_reach_only_the_application_owner() {
    let worker = Worker::with_application(
        ScriptedOwner::reading(Ok(Vec::new())),
        Some(ScriptedOwner::reading(Ok(Vec::new()))),
    );
    let without_applications = Worker::new(ScriptedOwner::reading(Ok(Vec::new())));
    let application = DeploymentLogTarget {
        owner_kind: DeploymentOwnerKind::Application as i32,
        ..target()
    };
    let stale_application = DeploymentLogTarget {
        manager_session_id: "b".repeat(32),
        ..application.clone()
    };

    let read = worker.read(read_request(application.clone())).await;
    let stale = worker.read(read_request(stale_application)).await;
    let unserved = without_applications.read(read_request(application)).await;

    assert!(matches!(
        read.outcome,
        Some(read_deployment_logs_response::Outcome::Histories(_))
    ));
    assert_eq!(
        worker
            .application
            .as_ref()
            .expect("application owner")
            .calls(),
        [DEPLOYMENT]
    );
    assert!(worker.owner.calls().is_empty());
    assert_eq!(
        rejection_reason(&stale),
        Some(DeploymentLogRejectionReason::ManagerSessionMismatch)
    );
    assert_eq!(
        rejection_reason(&unserved),
        Some(DeploymentLogRejectionReason::DeploymentNotFound)
    );
    assert!(without_applications.owner.calls().is_empty());
}

#[tokio::test]
async fn a_protected_value_spanning_the_entry_cut_is_redacted_before_the_cut() {
    const SECRET: &str = "launch-secret-0123456789";
    let mut content = vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES - 4];
    content.extend_from_slice(SECRET.as_bytes());
    content.extend_from_slice(b" tail");
    assert!(
        content[..MAX_LOG_ENTRY_CONTENT_BYTES].ends_with(&SECRET.as_bytes()[..4]),
        "a cut before redaction would keep the value's first bytes"
    );
    let line = entry(&model_source(), 1, content);
    let reading = Worker::new(
        ScriptedOwner::reading(Ok(vec![RuntimeLogSourceHistory {
            source: model_source(),
            result: Ok(RuntimeLogHistory {
                entries: vec![line.clone()],
                has_older_output: false,
                candidate_budget_truncated: false,
            }),
        }]))
        .protecting(&[SECRET]),
    );
    let following = Worker::new(ScriptedOwner::following(vec![line], true).protecting(&[SECRET]));

    let response = reading.read(read_request(target())).await;
    let mut follow = following.follow(follow_request()).await;
    next_frame(&mut follow).await;
    let followed = next_frame(&mut follow).await;

    let Some(read_deployment_logs_response::Outcome::Histories(histories)) = response.outcome
    else {
        panic!("the read is admitted");
    };
    let Some(deployment_log_source_history::Result::History(history)) =
        histories.sources[0].result.clone()
    else {
        panic!("the source is read");
    };
    let Some(follow_deployment_logs_response::Frame::Entry(followed)) = followed else {
        panic!("the followed entry is forwarded");
    };
    for sent in [&history.entries[0], followed.entry.as_ref().expect("entry")] {
        assert_eq!(sent.content.len(), MAX_LOG_ENTRY_CONTENT_BYTES);
        assert!(
            sent.content[..MAX_LOG_ENTRY_CONTENT_BYTES - 4]
                .iter()
                .all(|byte| *byte == b'x')
        );
        assert_eq!(
            String::from_utf8_lossy(&sent.content[MAX_LOG_ENTRY_CONTENT_BYTES - 4..]),
            "[RED",
            "the redaction marker, not the value, reaches the cut"
        );
        assert!(sent.truncated);
    }
}

#[tokio::test]
async fn a_protected_value_cut_by_the_runtime_never_leaves_the_worker() {
    const SECRET: &str = "launch-secret-0123456789";
    let mut cut = vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES - 6];
    cut.extend_from_slice(&SECRET.as_bytes()[..6]);
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(vec![RuntimeLogSourceHistory {
            source: model_source(),
            result: Ok(RuntimeLogHistory {
                entries: vec![RuntimeLogEntry {
                    truncated: true,
                    ..entry(&model_source(), 1, cut)
                }],
                has_older_output: false,
                candidate_budget_truncated: false,
            }),
        }]))
        .protecting(&[SECRET]),
    );

    let response = worker.read(read_request(target())).await;

    let Some(read_deployment_logs_response::Outcome::Histories(histories)) = response.outcome
    else {
        panic!("the read is admitted");
    };
    let Some(deployment_log_source_history::Result::History(history)) =
        histories.sources[0].result.clone()
    else {
        panic!("the source is read");
    };
    let sent = &history.entries[0];
    assert_eq!(
        String::from_utf8_lossy(&sent.content[MAX_LOG_ENTRY_CONTENT_BYTES - 10..]),
        "xxxx[REDAC"
    );
    assert!(sent.truncated);
}

#[tokio::test]
async fn entries_are_cut_to_the_entry_limit_and_the_oldest_are_dropped_to_the_budget() {
    let oversized = (0..80)
        .map(|second| entry(&model_source(), second, vec![b'x'; 70 * 1024]))
        .collect::<Vec<_>>();
    let worker = Worker::new(ScriptedOwner::reading(Ok(vec![
        RuntimeLogSourceHistory {
            source: model_source(),
            result: Ok(RuntimeLogHistory {
                entries: oversized,
                has_older_output: false,
                candidate_budget_truncated: false,
            }),
        },
        RuntimeLogSourceHistory {
            source: engine_source(),
            result: Err(RuntimeBackendError::workload_not_created()),
        },
    ])));

    let response = worker.read(read_request(target())).await;

    assert!(response.encoded_len() <= RESPONSE_BUDGET_BYTES);
    let Some(read_deployment_logs_response::Outcome::Histories(histories)) = response.outcome
    else {
        panic!("the read is admitted");
    };
    let Some(deployment_log_source_history::Result::History(model)) =
        histories.sources[0].result.clone()
    else {
        panic!("the model source is read");
    };
    assert!(model.candidate_budget_truncated);
    assert!(!model.has_older_output);
    let retained = model.entries.len();
    assert!((1..80).contains(&retained), "{retained} entries retained");
    assert_eq!(
        model
            .entries
            .iter()
            .map(|entry| entry.timestamp.map(|timestamp| timestamp.seconds))
            .collect::<Vec<_>>(),
        (80 - retained as i64..80).map(Some).collect::<Vec<_>>(),
        "the newest entries are kept in order"
    );
    assert!(
        model
            .entries
            .iter()
            .all(|entry| { entry.truncated && entry.content.len() == MAX_LOG_ENTRY_CONTENT_BYTES })
    );
    assert_eq!(
        histories.sources[1].result,
        Some(deployment_log_source_history::Result::Unavailable(
            super::DeploymentLogUnavailableReason::WorkloadNotCreated as i32
        ))
    );
}

#[tokio::test]
async fn requests_beyond_the_admission_limit_are_rejected_until_one_ends() {
    let worker = Worker::new(ScriptedOwner::following(Vec::new(), false));
    let mut follows = Vec::new();
    for _ in 0..8 {
        let mut follow = worker.follow(follow_request()).await;
        assert!(matches!(
            next_frame(&mut follow).await,
            Some(follow_deployment_logs_response::Frame::Opened(_))
        ));
        follows.push(follow);
    }

    let exhausted = worker.read(read_request(target())).await;
    drop(follows.pop());
    // The dropped follow releases its permit once the transport drops the response stream.
    let admitted = tokio::time::timeout(PROMPT, async {
        loop {
            let response = worker.read(read_request(target())).await;
            if rejection_reason(&response).is_none() {
                return response;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a released permit admits the next request");

    assert_eq!(
        rejection_reason(&exhausted),
        Some(DeploymentLogRejectionReason::CapacityExhausted)
    );
    assert!(matches!(
        admitted.outcome,
        Some(read_deployment_logs_response::Outcome::Histories(_))
    ));
}

#[tokio::test]
async fn dropping_a_follow_cancels_the_runtime_read() {
    let owner = ScriptedOwner::following(vec![entry(&model_source(), 1, b"one".to_vec())], false);
    let worker = Worker::new(owner);
    let mut follow = worker.follow(follow_request()).await;

    let opened = next_frame(&mut follow).await;
    let first = next_frame(&mut follow).await;
    let cancellation = worker.owner.follow_cancellations()[0].clone();
    assert!(!cancellation.is_cancelled());
    drop(follow);

    assert!(matches!(
        opened,
        Some(follow_deployment_logs_response::Frame::Opened(opened))
            if opened.sources.len() == 1 && opened.sources[0].unavailable.is_none()
    ));
    assert!(matches!(
        first,
        Some(follow_deployment_logs_response::Frame::Entry(entry))
            if entry.source_name == "model" && entry.entry.as_ref().is_some_and(|entry| entry.content == b"one")
    ));
    tokio::time::timeout(PROMPT, cancellation.cancelled())
        .await
        .expect("the runtime follow is cancelled with the call");
}

#[tokio::test]
async fn a_follow_of_every_source_announces_a_workload_created_after_it_opened_redacted() {
    let (created, created_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![
            Ok((
                Err(RuntimeBackendError::workload_not_created()),
                ProtectedLogValues::default(),
            )),
            Ok((
                Ok(created_stream),
                ProtectedLogValues::new(["launch-secret".to_owned()]),
            )),
        ]),
    );
    let mut follow = worker.follow(follow_request()).await;
    let opened = next_frame(&mut follow).await;

    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    feed(&created, 10, "at the resume position");
    feed(&created, 11, "key launch-secret");
    let announced = next_frames(&mut follow, 2).await;

    assert_eq!(
        opened,
        Some(opened_frame(
            &model_source(),
            Some(DeploymentLogUnavailableReason::WorkloadNotCreated)
        ))
    );
    assert_eq!(
        announced,
        [
            source_opened_frame(&model_source(), None),
            entry_frame(11, "key [REDACTED]"),
        ],
        "the workload replays after the resume position, redacted with its launch values"
    );
    assert_eq!(
        worker.owner.follow_source_names(),
        [Vec::new(), vec!["model".to_owned()]]
    );
    assert_eq!(
        worker.owner.follow_resume_positions()[1],
        BTreeMap::from([("model".to_owned(), UNIX_EPOCH + Duration::from_secs(10))])
    );
}

#[tokio::test]
async fn a_workload_announced_again_after_it_ended_repeats_no_delivered_line() {
    let (first, first_stream) = fed_stream();
    let (restarted, restarted_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![
            Ok((Ok(first_stream), ProtectedLogValues::default())),
            Ok((Ok(restarted_stream), ProtectedLogValues::default())),
        ]),
    );
    let mut follow = worker.follow(follow_request()).await;

    feed(&first, 11, "one");
    feed(&first, 12, "two");
    drop(first);
    let before = next_frames(&mut follow, 4).await;
    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    // A container restarted in place serves its earlier output again.
    feed(&restarted, 11, "one");
    feed(&restarted, 12, "two");
    feed(&restarted, 13, "three");
    let after = next_frames(&mut follow, 2).await;

    assert_eq!(
        before,
        [
            opened_frame(&model_source(), None),
            entry_frame(11, "one"),
            entry_frame(12, "two"),
            ended_frame(&model_source()),
        ]
    );
    assert_eq!(
        after,
        [
            source_opened_frame(&model_source(), None),
            entry_frame(13, "three"),
        ]
    );
    assert_eq!(
        worker.owner.follow_resume_positions()[1],
        BTreeMap::from([("model".to_owned(), UNIX_EPOCH + Duration::from_secs(12))]),
        "the source resumes after the last entry the follow delivered"
    );
}

#[tokio::test]
async fn a_replacement_opens_only_after_the_replaced_workload_ended() {
    let (old, old_stream) = fed_stream();
    let (new, new_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![
            Ok((Ok(old_stream), ProtectedLogValues::default())),
            Ok((Ok(new_stream), ProtectedLogValues::default())),
        ]),
    );
    let mut follow = worker.follow(follow_request()).await;
    feed(&old, 11, "old");
    let opened = next_frames(&mut follow, 2).await;

    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    feed(&new, 13, "new");
    feed(&old, 12, "old flushed");
    drop(old);
    let replaced = next_frames(&mut follow, 4).await;

    assert_eq!(
        opened,
        [opened_frame(&model_source(), None), entry_frame(11, "old")]
    );
    assert_eq!(
        replaced,
        [
            entry_frame(12, "old flushed"),
            ended_frame(&model_source()),
            source_opened_frame(&model_source(), None),
            entry_frame(13, "new"),
        ]
    );
    assert_eq!(
        worker.owner.follow_resume_positions()[1],
        BTreeMap::from([("model".to_owned(), UNIX_EPOCH + Duration::from_secs(12))]),
        "the replacement is resolved once the replaced workload ended, after its last line"
    );
}

#[tokio::test]
async fn a_follow_keeps_reading_workload_changes_while_the_manager_reads_slowly() {
    let (busy, busy_stream) = fed_stream();
    let (restarted, restarted_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![
            Ok((Ok(busy_stream), ProtectedLogValues::default())),
            Ok((Ok(restarted_stream), ProtectedLogValues::default())),
        ]),
    );
    let mut follow = worker.follow(follow_request()).await;
    let lines = (11..=210).collect::<Vec<u64>>();

    // The frames fill the follow's buffer while the manager reads none of them.
    for second in &lines {
        feed(&busy, *second, "line");
    }
    for _ in 0..100 {
        worker.owner.lifecycle.started(DEPLOYMENT, "model");
        tokio::task::yield_now().await;
    }
    drop(busy);
    feed(&restarted, 300, "restarted");
    let frames = next_frames(&mut follow, lines.len() + 4).await;

    let expected = std::iter::once(opened_frame(&model_source(), None))
        .chain(lines.iter().map(|second| entry_frame(*second, "line")))
        .chain([
            ended_frame(&model_source()),
            source_opened_frame(&model_source(), None),
            entry_frame(300, "restarted"),
        ])
        .collect::<Vec<_>>();
    assert_eq!(
        frames, expected,
        "no change was missed, and the requests made one reopening"
    );
    assert_eq!(worker.owner.follow_source_names().len(), 2);
}

#[tokio::test]
async fn a_workload_announced_again_opens_past_the_source_limit() {
    let steps = (1..MAX_RUNTIME_LOG_SOURCES)
        .map(|step| RuntimeLogSource {
            name: format!("prepare:step-{step}"),
            label: format!("Step {step}"),
        })
        .collect::<Vec<_>>();
    let (ended, ended_stream) = fed_stream();
    drop(ended);
    let (restarted, restarted_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new()))
            .starting_later(
                steps
                    .iter()
                    .map(|step| (step.clone(), Vec::new()))
                    .collect(),
            )
            .following_workloads(vec![
                Ok((Ok(ended_stream), ProtectedLogValues::default())),
                Ok((Ok(restarted_stream), ProtectedLogValues::default())),
            ]),
    );
    let mut follow = worker.follow(follow_request()).await;
    let opened = next_frame(&mut follow).await;
    let full = next_frames(&mut follow, MAX_RUNTIME_LOG_SOURCES).await;

    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    feed(&restarted, 11, "back");
    let announced = next_frames(&mut follow, 2).await;

    assert_eq!(opened, Some(opened_frame(&model_source(), None)));
    // Frames of different sources may interleave, so only their set is fixed.
    let expected = steps
        .iter()
        .map(|step| source_opened_frame(step, None))
        .chain([ended_frame(&model_source())])
        .collect::<Vec<_>>();
    assert_eq!(full.len(), expected.len());
    assert!(expected.iter().all(|frame| full.contains(frame)));
    assert_eq!(
        announced,
        [
            source_opened_frame(&model_source(), None),
            entry_frame(11, "back"),
        ]
    );
}

#[tokio::test]
async fn removing_the_deployment_ends_its_follow_once_its_open_sources_end() {
    let (running, running_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![Ok((
            Ok(running_stream),
            ProtectedLogValues::default(),
        ))]),
    );
    let mut follow = worker.follow(follow_request()).await;
    feed(&running, 11, "running");
    let opened = next_frames(&mut follow, 2).await;

    // A change still waiting when the deployment is removed opens nothing.
    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    worker.owner.lifecycle.removed(DEPLOYMENT);
    feed(&running, 12, "last");
    drop(running);
    let drained = next_frames(&mut follow, 2).await;
    let end = next_frame(&mut follow).await;

    assert_eq!(
        opened,
        [
            opened_frame(&model_source(), None),
            entry_frame(11, "running")
        ]
    );
    assert_eq!(
        drained,
        [entry_frame(12, "last"), ended_frame(&model_source())],
        "the last lines of a removed workload still arrive"
    );
    assert_eq!(end, None, "the follow ends without an error");
    assert_eq!(worker.owner.follow_source_names().len(), 1);
}

#[tokio::test]
async fn a_removed_deployments_follow_stops_sources_still_open_after_its_grace() {
    let (_running, running_stream) = fed_stream();
    let worker = Worker::with_removal_grace(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![Ok((
            Ok(running_stream),
            ProtectedLogValues::default(),
        ))]),
        Duration::ZERO,
    );
    let mut follow = worker.follow(follow_request()).await;
    let opened = next_frame(&mut follow).await;

    worker.owner.lifecycle.removed(DEPLOYMENT);
    let end = next_frame(&mut follow).await;

    assert_eq!(opened, Some(opened_frame(&model_source(), None)));
    assert_eq!(end, None, "the follow ends without an error");
    assert!(worker.owner.follow_cancellations()[0].is_cancelled());
}

#[tokio::test]
async fn a_workload_that_cannot_be_followed_again_ends_the_follow_with_an_error() {
    let (running, running_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![
            Ok((Ok(running_stream), ProtectedLogValues::default())),
            Err(DeploymentLogError::configuration(std::io::Error::other(
                "launch does not resolve",
            ))),
        ]),
    );
    let mut follow = worker.follow(follow_request()).await;
    let opened = next_frame(&mut follow).await;

    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    drop(running);
    let ended = next_frame(&mut follow).await;
    let failed = tokio::time::timeout(PROMPT, follow.next())
        .await
        .expect("failure deadline");
    let end = tokio::time::timeout(PROMPT, follow.next())
        .await
        .expect("end deadline");

    assert_eq!(opened, Some(opened_frame(&model_source(), None)));
    assert_eq!(ended, Some(ended_frame(&model_source())));
    assert!(matches!(
        failed,
        Some(Err(status)) if status.code() == tonic::Code::Internal
    ));
    assert!(end.is_none());
    assert!(worker.owner.follow_cancellations()[0].is_cancelled());
}

#[tokio::test]
async fn the_manager_reads_worker_logs_as_per_source_runtime_results() {
    let worker = Worker::new(ScriptedOwner::reading(Ok(vec![
        RuntimeLogSourceHistory {
            source: model_source(),
            result: Ok(RuntimeLogHistory {
                entries: vec![entry(&model_source(), 7, b"seven".to_vec())],
                has_older_output: true,
                candidate_budget_truncated: true,
            }),
        },
        RuntimeLogSourceHistory {
            source: engine_source(),
            result: Err(RuntimeBackendError::workload_not_created()),
        },
    ])));
    let listener = worker.listen().await;

    let histories = read_from(
        listener.connection(),
        request(&[model_source(), engine_source()]),
        &read_query(),
        PROMPT,
    )
    .await
    .expect("worker histories");

    assert_eq!(histories.len(), 2);
    assert_eq!(histories[0].source, model_source());
    assert_eq!(
        histories[0].result.as_ref().expect("model history"),
        &RuntimeLogHistory {
            entries: vec![entry(&model_source(), 7, b"seven".to_vec())],
            has_older_output: true,
            candidate_budget_truncated: true,
        },
        "a worker truncation stays truncated history"
    );
    assert!(matches!(
        histories[1].result,
        Err(RuntimeBackendError::WorkloadNotCreated)
    ));
}

#[tokio::test]
async fn a_manager_reading_every_source_reports_the_known_sources_the_worker_holds() {
    let step = RuntimeLogSource {
        name: "prepare:engine-build".to_owned(),
        label: "Engine build".to_owned(),
    };
    let history = |source: &RuntimeLogSource| RuntimeLogSourceHistory {
        source: RuntimeLogSource {
            label: "worker label".to_owned(),
            ..source.clone()
        },
        result: Ok(RuntimeLogHistory {
            entries: vec![entry(source, 3, b"three".to_vec())],
            has_older_output: false,
            candidate_budget_truncated: false,
        }),
    };
    let worker = Worker::new(ScriptedOwner::reading(Ok(vec![
        history(&step),
        history(&engine_source()),
        history(&model_source()),
    ])));
    let listener = worker.listen().await;
    let known = [step.clone(), model_source()];
    let defaults = [model_source()];
    let all = |node_id| NodeLogRequest {
        node_id,
        owner_kind: DeploymentOwnerKind::Model,
        deployment_id: DEPLOYMENT,
        selection: NodeLogSelection::All {
            known: &known,
            defaults: &defaults,
        },
    };
    let stale = WorkerConnection {
        node_incarnation: NodeIncarnation::parse("2".repeat(32)).expect("incarnation"),
        ..listener.connection()
    };

    let answered = read_from(
        listener.connection(),
        all(WORKER_NODE_ID),
        &read_query(),
        PROMPT,
    )
    .await
    .expect("worker histories");
    let unanswered = read_from(stale, all(WORKER_NODE_ID), &read_query(), PROMPT)
        .await
        .expect("an unavailable worker is not an error");

    // Unknown worker sources are dropped and known ones keep the owner's labels.
    assert_eq!(
        answered
            .iter()
            .map(|history| (history.source.clone(), history.result.is_ok()))
            .collect::<Vec<_>>(),
        [(step, true), (model_source(), true)]
    );
    assert_eq!(
        unanswered
            .iter()
            .map(|history| (
                history.source.clone(),
                matches!(
                    history.result,
                    Err(RuntimeBackendError::RemoteLogUnavailable)
                )
            ))
            .collect::<Vec<_>>(),
        [(model_source(), true)]
    );
}

#[tokio::test]
async fn a_follow_of_every_source_receives_known_sources_the_worker_starts_later() {
    let step = RuntimeLogSource {
        name: "prepare:engine-build".to_owned(),
        label: "Engine build".to_owned(),
    };
    let stray = RuntimeLogSource {
        name: "stray".to_owned(),
        label: "Stray".to_owned(),
    };
    let (first, first_stream) = fed_stream();
    let (restarted, restarted_stream) = fed_stream();
    let worker = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new()))
            .following_workloads(vec![
                Ok((Ok(first_stream), ProtectedLogValues::default())),
                Ok((Ok(restarted_stream), ProtectedLogValues::default())),
            ])
            .starting_later(vec![
                (stray.clone(), vec![entry(&stray, 1, b"never".to_vec())]),
                (
                    RuntimeLogSource {
                        label: "worker label".to_owned(),
                        ..step.clone()
                    },
                    vec![entry(&step, 2, b"building".to_vec())],
                ),
            ]),
    );
    let listener = worker.listen().await;
    let known = [step.clone(), model_source()];
    let cancellation = CancellationToken::new();
    let streams = follow_from(
        listener.connection(),
        every_source(&known),
        &follow_query(cancellation.clone()),
        not_present(),
        PROMPT,
    )
    .await
    .expect("worker follow");
    let opened = streams
        .sources
        .iter()
        .map(|source| source.source.clone())
        .collect::<Vec<_>>();
    let mut events = Box::pin(follow_sources(
        streams,
        ProtectedLogValues::default(),
        cancellation.clone(),
        DeploymentLogDiagnosticContext::new("model", DEPLOYMENT, "follow"),
    ));

    feed(&first, 11, "first");
    drop(first);
    let mut received = next_events(&mut events, 5).await;
    worker.owner.lifecycle.started(DEPLOYMENT, "model");
    feed(&restarted, 11, "first");
    feed(&restarted, 12, "restarted");
    received.extend(next_events(&mut events, 2).await);
    cancellation.cancel();

    // The unknown source is dropped, and the known one keeps the owner's label.
    assert_eq!(opened, [model_source()]);
    assert_eq!(
        of_source(&received, &model_source()),
        [
            "model available Model",
            "model entry first",
            "model ended",
            "model available Model",
            "model entry restarted",
        ],
        "the restarted workload follows its ended stream, without repeating its lines"
    );
    assert_eq!(
        of_source(&received, &step),
        [
            "prepare:engine-build available Engine build",
            "prepare:engine-build entry building",
        ]
    );
}

#[tokio::test]
async fn stale_hung_or_unreachable_workers_leave_sources_unavailable() {
    let worker = Worker::new(ScriptedOwner::hanging());
    let listener = worker.listen().await;
    let stale = WorkerConnection {
        node_incarnation: NodeIncarnation::parse("2".repeat(32)).expect("incarnation"),
        ..listener.connection()
    };
    let closed = reserve_port();
    let unreachable = WorkerConnection {
        channel: listener
            .channels
            .channel(
                WORKER_NODE_ID,
                WorkerChannelPurpose::Diagnostics,
                closed_address(&closed).into(),
            )
            .expect("channel"),
        ..listener.connection()
    };
    let sources = [model_source()];

    let outcomes = [
        read_from(stale, request(&sources), &read_query(), PROMPT).await,
        read_from(
            listener.connection(),
            request(&sources),
            &read_query(),
            Duration::from_millis(200),
        )
        .await,
        read_from(unreachable, request(&sources), &read_query(), PROMPT).await,
    ];

    for outcome in outcomes {
        let histories = outcome.expect("an unavailable worker is not an error");
        assert_eq!(histories.len(), 1);
        assert_eq!(histories[0].source, model_source());
        assert!(matches!(
            histories[0].result,
            Err(RuntimeBackendError::RemoteLogUnavailable)
        ));
    }
    assert_eq!(worker.owner.calls(), [DEPLOYMENT]);
}

#[tokio::test]
async fn a_deployment_the_worker_has_not_installed_reads_as_not_created_yet() {
    let not_installed = Worker::new(ScriptedOwner::reading(Err(DeploymentLogError::NotFound)));
    let sources = [model_source()];

    let history = read_from(
        not_installed.listen().await.connection(),
        request(&sources),
        &read_query(),
        PROMPT,
    )
    .await
    .expect("a deployment the worker has not installed yet is not an error");

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].source, model_source());
    assert!(matches!(
        history[0].result,
        Err(RuntimeBackendError::WorkloadNotCreated)
    ));
}

#[tokio::test]
async fn a_follow_the_worker_cannot_open_fails() {
    let hung = Worker::new(ScriptedOwner::hanging());
    let hung_listener = hung.listen().await;
    let stale = Worker::new(ScriptedOwner::following(Vec::new(), false));
    let stale_listener = stale.listen().await;
    let unresolved = Worker::new(ScriptedOwner::reading(Err(
        DeploymentLogError::configuration(std::io::Error::other("launch does not resolve")),
    )));
    let not_installed = Worker::new(ScriptedOwner::reading(Err(DeploymentLogError::NotFound)));
    let closed = reserve_port();
    let unreachable = WorkerConnection {
        channel: stale_listener
            .channels
            .channel(
                WORKER_NODE_ID,
                WorkerChannelPurpose::Diagnostics,
                closed_address(&closed).into(),
            )
            .expect("channel"),
        ..stale_listener.connection()
    };
    let known = [model_source()];

    let outcomes = [
        (
            "timed out",
            follow_from(
                hung_listener.connection(),
                every_source(&known),
                &follow_query(CancellationToken::new()),
                not_present(),
                Duration::from_millis(200),
            )
            .await,
        ),
        (
            "stale",
            follow_from(
                WorkerConnection {
                    node_incarnation: NodeIncarnation::parse("2".repeat(32)).expect("incarnation"),
                    ..stale_listener.connection()
                },
                every_source(&known),
                &follow_query(CancellationToken::new()),
                not_present(),
                PROMPT,
            )
            .await,
        ),
        (
            "unreachable",
            follow_from(
                unreachable,
                every_source(&known),
                &follow_query(CancellationToken::new()),
                not_present(),
                PROMPT,
            )
            .await,
        ),
        (
            "unresolved",
            follow_from(
                unresolved.listen().await.connection(),
                every_source(&known),
                &follow_query(CancellationToken::new()),
                not_present(),
                PROMPT,
            )
            .await,
        ),
        (
            "not installed",
            follow_from(
                not_installed.listen().await.connection(),
                every_source(&known),
                &follow_query(CancellationToken::new()),
                not_present(),
                PROMPT,
            )
            .await,
        ),
    ];

    for (case, outcome) in outcomes {
        assert!(
            matches!(outcome, Err(DeploymentLogError::NodeUnavailable)),
            "{case}: {:?}",
            outcome.err()
        );
    }
    assert_eq!(hung.owner.calls(), [DEPLOYMENT]);
    assert_eq!(stale.owner.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_follow_the_worker_breaks_ends_with_an_error_and_a_removal_ends_it_cleanly() {
    let (broken_feed, broken_stream) = fed_stream();
    let (removed, removed_stream) = fed_stream();
    let broken = Worker::new(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![
            Ok((Ok(broken_stream), ProtectedLogValues::default())),
            Err(DeploymentLogError::configuration(std::io::Error::other(
                "launch does not resolve",
            ))),
        ]),
    );
    // The workload of a removed deployment is stopped at once, as a source that never ends is.
    let removing = Worker::with_removal_grace(
        ScriptedOwner::reading(Ok(Vec::new())).following_workloads(vec![Ok((
            Ok(removed_stream),
            ProtectedLogValues::default(),
        ))]),
        Duration::ZERO,
    );
    let known = [model_source()];
    let mut broken_events = Box::pin(follow_sources(
        follow_from(
            broken.listen().await.connection(),
            every_source(&known),
            &follow_query(CancellationToken::new()),
            not_present(),
            PROMPT,
        )
        .await
        .expect("worker follow"),
        ProtectedLogValues::default(),
        CancellationToken::new(),
        DeploymentLogDiagnosticContext::new("model", DEPLOYMENT, "follow"),
    ));
    let mut removed_events = Box::pin(follow_sources(
        follow_from(
            removing.listen().await.connection(),
            every_source(&known),
            &follow_query(CancellationToken::new()),
            not_present(),
            PROMPT,
        )
        .await
        .expect("worker follow"),
        ProtectedLogValues::default(),
        CancellationToken::new(),
        DeploymentLogDiagnosticContext::new("model", DEPLOYMENT, "follow"),
    ));

    let broken_opened = next_events(&mut broken_events, 1).await;
    // The broken source opens again only once its stream ended.
    broken.owner.lifecycle.started(DEPLOYMENT, "model");
    drop(broken_feed);
    let broken_rest = tokio::time::timeout(PROMPT, broken_events.collect::<Vec<_>>())
        .await
        .expect("the broken follow ends");
    feed(&removed, 11, "running");
    let removed_opened = next_events(&mut removed_events, 2).await;
    removing.owner.lifecycle.removed(DEPLOYMENT);
    let removed_rest = tokio::time::timeout(PROMPT, removed_events.collect::<Vec<_>>())
        .await
        .expect("the removed follow ends");

    assert_eq!(
        of_source(&broken_opened, &model_source()),
        ["model available Model"]
    );
    assert!(
        matches!(
            broken_rest.as_slice(),
            [Ok(DeploymentLogFollowEvent::Ended(source)), Err(status)]
                if *source == model_source() && status.code() == tonic::Code::Unavailable
        ),
        "a broken follow ends with an error after its ended source: {broken_rest:?}"
    );
    assert_eq!(
        of_source(&removed_opened, &model_source()),
        ["model available Model", "model entry running"]
    );
    assert_eq!(
        removed_rest
            .into_iter()
            .map(|event| event.map(|event| of_source(&[event], &model_source())))
            .collect::<Result<Vec<_>, _>>()
            .expect("a removal is no error"),
        [vec!["model ended".to_owned()]],
        "a removal ends the open source, then the follow, cleanly"
    );
}

#[tokio::test]
async fn a_follow_the_worker_drops_while_the_manager_still_wants_it_running_ends_with_an_error() {
    let (moved, moved_stream) = fed_stream();
    let worker = Worker::with_removal_grace(
        ScriptedOwner::reading(Ok(Vec::new()))
            .following_workloads(vec![Ok((Ok(moved_stream), ProtectedLogValues::default()))]),
        Duration::ZERO,
    );
    let known = [model_source()];
    let mut events = Box::pin(follow_sources(
        follow_from(
            worker.listen().await.connection(),
            every_source(&known),
            &follow_query(CancellationToken::new()),
            Arc::new(|| true),
            PROMPT,
        )
        .await
        .expect("worker follow"),
        ProtectedLogValues::default(),
        CancellationToken::new(),
        DeploymentLogDiagnosticContext::new("model", DEPLOYMENT, "follow"),
    ));

    feed(&moved, 11, "running");
    let opened = next_events(&mut events, 2).await;
    worker.owner.lifecycle.removed(DEPLOYMENT);
    let rest = tokio::time::timeout(PROMPT, events.collect::<Vec<_>>())
        .await
        .expect("the follow ends");

    assert_eq!(
        of_source(&opened, &model_source()),
        ["model available Model", "model entry running"]
    );
    assert!(
        matches!(
            rest.as_slice(),
            [Err(status)] if status.code() == tonic::Code::Unavailable
        ),
        "a deployment moved to another node is reopened, its source not ended: {rest:?}"
    );
}

#[tokio::test]
async fn closing_a_follow_cancels_the_worker_read_and_releases_both_permits() {
    let worker = Worker::new(ScriptedOwner::following(Vec::new(), false));
    let listener = worker.listen().await;
    let manager = ManagementLogAdmission::new();
    let permit = manager.try_acquire().expect("manager permit");
    let known = [model_source()];
    let cancellation = CancellationToken::new();
    let streams = follow_from(
        listener.connection(),
        every_source(&known),
        &follow_query(cancellation.clone()),
        not_present(),
        PROMPT,
    )
    .await
    .expect("worker follow");
    let mut response = follow_response(
        streams,
        ProtectedLogValues::default(),
        DeploymentLogDiagnosticContext::new("model", DEPLOYMENT, "follow"),
        cancellation,
        permit,
        |event| event,
    );
    let opened = next_events(&mut response, 1).await;
    let worker_cancellation = worker.owner.follow_cancellations()[0].clone();
    assert!(!worker_cancellation.is_cancelled());

    drop(response);

    assert_eq!(
        of_source(&opened, &model_source()),
        ["model available Model"]
    );
    tokio::time::timeout(PROMPT, async {
        worker_cancellation.cancelled().await;
        manager.all_released().await;
        worker.admission.all_released().await;
    })
    .await
    .expect("closing the follow stops the worker read and releases both permits");
}

#[tokio::test]
async fn an_unknown_source_is_an_error_for_the_manager() {
    let invalid = Worker::new(ScriptedOwner::reading(Err(
        DeploymentLogError::InvalidSource,
    )));
    let sources = [model_source()];

    let invalid = read_from(
        invalid.listen().await.connection(),
        request(&sources),
        &read_query(),
        PROMPT,
    )
    .await;

    assert!(matches!(invalid, Err(DeploymentLogError::InvalidSource)));
}

#[tokio::test]
async fn a_forwarded_follow_streams_entries_and_ends_or_stops_the_worker_when_cancelled() {
    let ending = Worker::new(ScriptedOwner::following(
        vec![entry(&model_source(), 11, b"eleven".to_vec())],
        true,
    ));
    let holding = Worker::new(ScriptedOwner::following(Vec::new(), false));
    let sources = [model_source()];
    let ending_query = follow_query(CancellationToken::new());
    let holding_cancellation = CancellationToken::new();

    let ended = follow_from(
        ending.listen().await.connection(),
        request(&sources),
        &ending_query,
        not_present(),
        PROMPT,
    )
    .await
    .expect("worker follow")
    .sources
    .pop()
    .expect("one source")
    .result
    .expect("an open source");
    let held = follow_from(
        holding.listen().await.connection(),
        request(&sources),
        &follow_query(holding_cancellation.clone()),
        not_present(),
        PROMPT,
    )
    .await
    .expect("worker follow");

    let events = tokio::time::timeout(PROMPT, ended.collect::<Vec<_>>())
        .await
        .expect("the ended source's stream ends");
    assert!(matches!(
        events.as_slice(),
        [Ok(RuntimeLogEvent::Entry(entry))]
            if entry == &super::tests::entry(&model_source(), 11, b"eleven".to_vec())
    ));
    assert_eq!(
        ending.owner.follow_resume_positions(),
        [BTreeMap::from([(
            "model".to_owned(),
            UNIX_EPOCH + Duration::from_secs(10)
        )])],
        "the worker resumes where the manager left off"
    );
    assert!(held.sources[0].result.is_ok());
    let worker_cancellation = holding.owner.follow_cancellations()[0].clone();
    assert!(!worker_cancellation.is_cancelled());
    holding_cancellation.cancel();
    tokio::time::timeout(PROMPT, worker_cancellation.cancelled())
        .await
        .expect("cancelling the manager follow stops the worker read");
}

#[tokio::test]
async fn a_follow_of_every_source_hands_every_resume_position_to_the_worker_owner() {
    let worker = Worker::new(ScriptedOwner::following(Vec::new(), false));
    let known = [model_source()];
    let positions = BTreeMap::from([
        ("model".to_owned(), UNIX_EPOCH + Duration::new(10, 1)),
        (
            "prepare:download".to_owned(),
            UNIX_EPOCH + Duration::new(3, 2),
        ),
        (
            "prepare:engine-build".to_owned(),
            UNIX_EPOCH + Duration::new(4, 3),
        ),
    ]);
    let cancellation = CancellationToken::new();

    follow_from(
        worker.listen().await.connection(),
        every_source(&known),
        &DeploymentLogFollowQuery::new(Vec::new(), positions.clone(), cancellation.clone()),
        not_present(),
        PROMPT,
    )
    .await
    .expect("worker follow");
    cancellation.cancel();

    assert_eq!(worker.owner.follow_resume_positions(), [positions]);
}

#[tokio::test]
async fn inference_and_diagnostics_reach_a_worker_over_separate_connections() {
    let worker = Worker::new(ScriptedOwner::reading(Ok(vec![RuntimeLogSourceHistory {
        source: model_source(),
        result: Ok(RuntimeLogHistory {
            entries: Vec::new(),
            has_older_output: false,
            candidate_budget_truncated: false,
        }),
    }])));
    let listener = worker.listen().await;
    let proxy = CountingProxy::start(listener.address).await;
    let sources = [model_source()];

    for purpose in [
        WorkerChannelPurpose::Inference,
        WorkerChannelPurpose::Diagnostics,
        WorkerChannelPurpose::Inference,
        WorkerChannelPurpose::Diagnostics,
    ] {
        let connection = WorkerConnection {
            channel: listener
                .channels
                .channel(WORKER_NODE_ID, purpose, proxy.address.into())
                .expect("worker channel"),
            manager_session: manager_session(),
            node_incarnation: node_incarnation(),
        };
        let histories = read_from(connection, request(&sources), &read_query(), PROMPT)
            .await
            .expect("worker histories");
        assert!(
            histories[0].result.is_ok(),
            "{purpose:?} reaches the worker"
        );
    }

    assert_eq!(
        proxy.connections(),
        2,
        "each purpose keeps one reused connection of its own"
    );
}

/// A worker's log service over an installed view assigning `DEPLOYMENT`, with a scripted model
/// owner and an optional scripted application owner.
struct Worker {
    view: AcceptedNodeViewAuthority,
    owner: Arc<ScriptedOwner>,
    application: Option<Arc<ScriptedOwner>>,

    /// Admission of the worker's listener.
    admission: ManagementLogAdmission,

    /// Longest a follow of a removed deployment waits for its open sources, when not the
    /// default.
    removal_grace: Option<Duration>,
    service: DeploymentLogService,
}

impl Worker {
    fn new(owner: ScriptedOwner) -> Self {
        Self::with_application(owner, None)
    }

    /// A worker whose follows of a removed deployment wait at most `grace` for open sources.
    fn with_removal_grace(owner: ScriptedOwner, grace: Duration) -> Self {
        let worker = Self::new(owner);
        Self {
            service: DeploymentLogService::new(
                DeploymentLogReader::new(
                    worker.view.clone(),
                    owners(&worker.owner, None),
                    ManagementLogAdmission::new(),
                )
                .with_removal_grace(grace),
                &PrivateServiceContext::new(PrivateNodeRole::Worker, WORKER_NODE_ID),
            ),
            removal_grace: Some(grace),
            ..worker
        }
    }

    fn with_application(owner: ScriptedOwner, application: Option<ScriptedOwner>) -> Self {
        let view = installed_view();
        let owner = Arc::new(owner);
        let application = application.map(Arc::new);
        Self {
            service: DeploymentLogService::new(
                DeploymentLogReader::new(
                    view.clone(),
                    owners(&owner, application.as_ref()),
                    ManagementLogAdmission::new(),
                ),
                &PrivateServiceContext::new(PrivateNodeRole::Worker, WORKER_NODE_ID),
            ),
            view,
            owner,
            application,
            admission: ManagementLogAdmission::new(),
            removal_grace: None,
        }
    }

    async fn read(&self, request: ReadDeploymentLogsRequest) -> ReadDeploymentLogsResponse {
        self.service
            .read_deployment_logs(authenticated(request, Some(PrivateNodeRole::Manager)))
            .await
            .expect("authorized read")
            .into_inner()
    }

    async fn follow(
        &self,
        request: FollowDeploymentLogsRequest,
    ) -> std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>>
                + Send,
        >,
    > {
        self.service
            .follow_deployment_logs(authenticated(request, Some(PrivateNodeRole::Manager)))
            .await
            .expect("authorized follow")
            .into_inner()
    }

    /// Serves this worker's view and owner on a real mutual-TLS private listener.
    async fn listen(&self) -> Listener {
        let authority = TestAuthority::new();
        let (worker_certificate, worker_key) = authority.issue(SITE_ID, WORKER_NODE_ID, true);
        let manager = authority.issue(SITE_ID, MANAGER_NODE_ID, false);
        let view = self.view.clone();
        let owners = owners(&self.owner, self.application.as_ref());
        let admission = self.admission.clone();
        let removal_grace = self.removal_grace;
        let server = crate::server::private::prepare(
            authority.private_configuration(
                SITE_ID,
                WORKER_NODE_ID,
                PrivateNodeRole::Worker,
                (worker_certificate.clone(), worker_key),
                [(
                    MANAGER_NODE_ID,
                    PrivateNodeRole::Manager,
                    manager.0.as_slice(),
                )],
            ),
            &Timing::test().private,
            PrivateServiceAssembly::new(move |context, routes| {
                routes.add_service(
                    DeploymentLogService::new(
                        {
                            let reader = DeploymentLogReader::new(view, owners, admission);
                            match removal_grace {
                                Some(grace) => reader.with_removal_grace(grace),
                                None => reader,
                            }
                        },
                        &context,
                    )
                    .into_server(),
                );
            })
            .assemble(PrivateNodeRole::Worker, WORKER_NODE_ID),
            std::future::pending(),
        )
        .await
        .expect("worker private listener");
        let address = server.address;
        tokio::spawn(server.serve());
        Listener {
            address,
            channels: WorkerChannels::new(
                &authority.private_configuration(
                    SITE_ID,
                    MANAGER_NODE_ID,
                    PrivateNodeRole::Manager,
                    manager,
                    [(
                        WORKER_NODE_ID,
                        PrivateNodeRole::Worker,
                        worker_certificate.as_slice(),
                    )],
                ),
                &Timing::test().private,
            ),
        }
    }
}

fn owners(
    model: &Arc<ScriptedOwner>,
    application: Option<&Arc<ScriptedOwner>>,
) -> DeploymentLogOwners {
    DeploymentLogOwners {
        model: Arc::clone(model) as Arc<dyn DeploymentLogOwner>,
        application: application.map(|owner| Arc::clone(owner) as Arc<dyn DeploymentLogOwner>),
    }
}

/// A worker's private listener and the manager's pinned channels to it.
struct Listener {
    address: SocketAddr,
    channels: WorkerChannels,
}

impl Listener {
    fn connection(&self) -> WorkerConnection {
        WorkerConnection {
            channel: self
                .channels
                .channel(
                    WORKER_NODE_ID,
                    WorkerChannelPurpose::Diagnostics,
                    self.address.into(),
                )
                .expect("worker channel"),
            manager_session: manager_session(),
            node_incarnation: node_incarnation(),
        }
    }
}

/// Deployment-log owner that answers reads from a script and serves one scripted follow source.
struct ScriptedOwner {
    read: Mutex<Option<Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>>>,
    hang: bool,
    follow_entries: Vec<RuntimeLogEntry>,
    follow_ends: bool,
    protected: ProtectedLogValues,
    calls: Mutex<Vec<String>>,
    follows: Mutex<Vec<DeploymentLogFollowQuery>>,

    /// Sources a follow lists after it opened, each with the entries it then serves.
    late_sources: Vec<(RuntimeLogSource, Vec<RuntimeLogEntry>)>,

    /// Model-source follows each follow call takes in turn, before the scripted entries.
    workloads: Mutex<VecDeque<ScriptedFollow>>,

    /// Workload changes a follow of every source receives.
    lifecycle: WorkloadLifecycle,
}

/// One scripted follow of the model source: its stream, or the runtime failure it reads as,
/// with the values protected for it; or the owner failure the follow call returns.
type ScriptedFollow = Result<
    (
        Result<RuntimeLogStream, RuntimeBackendError>,
        ProtectedLogValues,
    ),
    DeploymentLogError,
>;

impl ScriptedOwner {
    fn reading(read: Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>) -> Self {
        Self {
            read: Mutex::new(Some(read)),
            hang: false,
            follow_entries: Vec::new(),
            follow_ends: true,
            protected: ProtectedLogValues::default(),
            calls: Mutex::default(),
            follows: Mutex::default(),
            late_sources: Vec::new(),
            workloads: Mutex::default(),
            lifecycle: WorkloadLifecycle::new(),
        }
    }

    fn hanging() -> Self {
        Self {
            hang: true,
            ..Self::reading(Ok(Vec::new()))
        }
    }

    fn following(entries: Vec<RuntimeLogEntry>, ends: bool) -> Self {
        Self {
            follow_entries: entries,
            follow_ends: ends,
            ..Self::reading(Ok(Vec::new()))
        }
    }

    /// Lists each of `sources` after the follow opened, serving its entries.
    fn starting_later(self, sources: Vec<(RuntimeLogSource, Vec<RuntimeLogEntry>)>) -> Self {
        Self {
            late_sources: sources,
            ..self
        }
    }

    /// Serves each of `follows` to one follow call in turn, the first to the opening one.
    fn following_workloads(self, follows: Vec<ScriptedFollow>) -> Self {
        Self {
            workloads: Mutex::new(follows.into()),
            ..self
        }
    }

    /// Answers with `values` as the deployment's protected launch values.
    fn protecting(self, values: &[&str]) -> Self {
        Self {
            protected: ProtectedLogValues::new(values.iter().map(|value| (*value).to_owned())),
            ..self
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("owner calls").clone()
    }

    fn follow_cancellations(&self) -> Vec<CancellationToken> {
        self.follows
            .lock()
            .expect("owner follows")
            .iter()
            .map(|query| query.cancellation().clone())
            .collect()
    }

    fn follow_resume_positions(&self) -> Vec<BTreeMap<String, std::time::SystemTime>> {
        self.follows
            .lock()
            .expect("owner follows")
            .iter()
            .map(|query| query.resume_positions().clone())
            .collect()
    }

    fn follow_source_names(&self) -> Vec<Vec<String>> {
        self.follows
            .lock()
            .expect("owner follows")
            .iter()
            .map(|query| query.source_names().to_vec())
            .collect()
    }

    /// The model source serving the scripted entries, then ending or holding until cancelled.
    fn entries_stream(&self, cancellation: CancellationToken) -> RuntimeLogStream {
        let entries = futures_util::stream::iter(
            self.follow_entries
                .clone()
                .into_iter()
                .map(|entry| Ok(RuntimeLogEvent::Entry(entry))),
        );
        if self.follow_ends {
            Box::pin(entries)
        } else {
            Box::pin(entries.chain(futures_util::stream::once(async move {
                cancellation.cancelled().await;
                Err(RuntimeBackendError::LogCancelled)
            })))
        }
    }
}

impl DeploymentLogOwner for ScriptedOwner {
    fn read_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        _: DeploymentLogReadQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceHistory>> {
        self.calls
            .lock()
            .expect("owner calls")
            .push(deployment_id.to_owned());
        let scripted = if self.hang {
            None
        } else {
            let mut read = self.read.lock().expect("owner read");
            match read.as_ref() {
                // Each read gets its own copy of a successful script.
                Some(Ok(_)) => read.as_ref().map(clone_histories),
                Some(Err(_)) => read.take(),
                None => Some(Ok(Vec::new())),
            }
        };
        let protected = self.protected.clone();
        Box::pin(async move {
            match scripted {
                Some(result) => {
                    result.map(|sources| OwnerLogSources::protected(sources, protected))
                }
                None => std::future::pending().await,
            }
        })
    }

    fn follow_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogFollowQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceStream>> {
        self.calls
            .lock()
            .expect("owner calls")
            .push(deployment_id.to_owned());
        let every_source = query.source_names().is_empty();
        let changes = every_source.then(|| self.lifecycle.subscribe(deployment_id));
        // A scripted refusal applies to follows too.
        let mut read = self.read.lock().expect("owner read");
        if matches!(read.as_ref(), Some(Err(_))) {
            let Some(Err(error)) = read.take() else {
                unreachable!("checked above");
            };
            return Box::pin(std::future::ready(Err(error)));
        }
        drop(read);
        if self.hang {
            return Box::pin(std::future::pending());
        }
        let cancellation = query.cancellation().clone();
        let late_cancellation = cancellation.clone();
        self.follows.lock().expect("owner follows").push(query);
        let (result, protected) = match self.workloads.lock().expect("workloads").pop_front() {
            Some(Ok(follow)) => follow,
            Some(Err(error)) => return Box::pin(std::future::ready(Err(error))),
            None => (
                Ok(self.entries_stream(cancellation)),
                self.protected.clone(),
            ),
        };
        let late = (every_source && !self.late_sources.is_empty()).then(|| {
            let (sender, receiver) = tokio::sync::mpsc::channel(self.late_sources.len());
            for (source, entries) in &self.late_sources {
                let cancellation = late_cancellation.clone();
                let stream: RuntimeLogStream = Box::pin(
                    futures_util::stream::iter(
                        entries
                            .clone()
                            .into_iter()
                            .map(|entry| Ok(RuntimeLogEvent::Entry(entry))),
                    )
                    .chain(futures_util::stream::once(async move {
                        cancellation.cancelled().await;
                        Err(RuntimeBackendError::LogCancelled)
                    })),
                );
                sender
                    .try_send(RuntimeLogSourceStream {
                        source: source.clone(),
                        result: Ok(stream),
                    })
                    .expect("late source fits its channel");
            }
            receiver
        });
        Box::pin(std::future::ready(Ok(OwnerLogSources::protected(
            vec![RuntimeLogSourceStream {
                source: model_source(),
                result,
            }],
            protected,
        )
        .with_late(late)
        .with_changes(changes))))
    }
}

fn clone_histories(
    scripted: &Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError>,
) -> Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError> {
    let Ok(histories) = scripted else {
        unreachable!("only successful scripts are cloned");
    };
    Ok(histories
        .iter()
        .map(|history| RuntimeLogSourceHistory {
            source: history.source.clone(),
            result: match &history.result {
                Ok(history) => Ok(history.clone()),
                Err(RuntimeBackendError::WorkloadNotCreated) => {
                    Err(RuntimeBackendError::workload_not_created())
                }
                Err(_) => Err(RuntimeBackendError::remote_log_unavailable()),
            },
        })
        .collect())
}

fn installed_view() -> AcceptedNodeViewAuthority {
    let view = AcceptedNodeViewAuthority::new(
        SITE_ID,
        WORKER_NODE_ID,
        node_incarnation(),
        manager_session(),
        Duration::from_secs(30),
    )
    .expect("view authority");
    let projection = NodeViewProjection::new(
        vec![ExecutionTarget {
            execution_target_id: "target-a".to_owned(),
            site_node_id: WORKER_NODE_ID.to_owned(),
            ..Default::default()
        }],
        vec![ModelDeploymentDeclaration {
            model_deployment_id: DEPLOYMENT.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: WORKER_NODE_ID.to_owned(),
                execution_target_id: "target-a".to_owned(),
            }),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            lifecycle_generation: 1,
            ..Default::default()
        }],
    )
    .expect("node view projection");
    view.install_for_test(SITE_ID, WORKER_NODE_ID, 1, &projection)
        .expect("installed view");
    view
}

fn target() -> DeploymentLogTarget {
    DeploymentLogTarget {
        manager_session_id: manager_session().as_str().to_owned(),
        node_incarnation_id: node_incarnation().as_str().to_owned(),
        owner_kind: DeploymentOwnerKind::Model as i32,
        deployment_id: DEPLOYMENT.to_owned(),
    }
}

fn read_request(target: DeploymentLogTarget) -> ReadDeploymentLogsRequest {
    ReadDeploymentLogsRequest {
        target: Some(target),
        source_names: Vec::new(),
        maximum_entries: 200,
        before_timestamp: None,
    }
}

fn follow_request() -> FollowDeploymentLogsRequest {
    FollowDeploymentLogsRequest {
        target: Some(target()),
        source_names: Vec::new(),
        resume_positions: vec![DeploymentLogResumePosition {
            source_name: "model".to_owned(),
            latest_timestamp: Some(super::timestamp(UNIX_EPOCH + Duration::from_secs(10))),
        }],
    }
}

/// A follow request for every source of the deployment among `known`.
fn every_source(known: &[RuntimeLogSource]) -> NodeLogRequest<'_> {
    const DEFAULTS: &[RuntimeLogSource] = &[];
    NodeLogRequest {
        node_id: WORKER_NODE_ID,
        owner_kind: DeploymentOwnerKind::Model,
        deployment_id: DEPLOYMENT,
        selection: NodeLogSelection::All {
            known,
            defaults: DEFAULTS,
        },
    }
}

/// The manager no longer wants the deployment running, so a follow the worker ends was a
/// removal.
fn not_present() -> StillPresent {
    Arc::new(|| false)
}

fn request(sources: &[RuntimeLogSource]) -> NodeLogRequest<'_> {
    NodeLogRequest {
        node_id: WORKER_NODE_ID,
        owner_kind: DeploymentOwnerKind::Model,
        deployment_id: DEPLOYMENT,
        selection: NodeLogSelection::Named(sources),
    }
}

fn read_query() -> DeploymentLogReadQuery {
    DeploymentLogReadQuery::new(
        Vec::new(),
        RuntimeLogQuery::new(NonZeroU16::new(200).expect("limit"), None).expect("query"),
    )
}

fn follow_query(cancellation: CancellationToken) -> DeploymentLogFollowQuery {
    DeploymentLogFollowQuery::new(
        Vec::new(),
        BTreeMap::from([("model".to_owned(), UNIX_EPOCH + Duration::from_secs(10))]),
        cancellation,
    )
}

fn authenticated<T>(message: T, role: Option<PrivateNodeRole>) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(role) = role {
        request
            .extensions_mut()
            .insert(AuthenticatedMachineIdentity {
                site_id: SITE_ID.to_owned(),
                node_id: MANAGER_NODE_ID.to_owned(),
                role,
            });
    }
    request
}

fn rejection_reason(response: &ReadDeploymentLogsResponse) -> Option<DeploymentLogRejectionReason> {
    match &response.outcome {
        Some(read_deployment_logs_response::Outcome::Rejected(rejected)) => {
            DeploymentLogRejectionReason::try_from(rejected.reason).ok()
        }
        _ => None,
    }
}

async fn next_frame(
    follow: &mut (
             impl tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>>
             + Unpin
         ),
) -> Option<follow_deployment_logs_response::Frame> {
    tokio::time::timeout(PROMPT, follow.next())
        .await
        .expect("frame arrives promptly")
        .map(|frame| frame.expect("follow frame").frame.expect("frame payload"))
}

async fn next_frames(
    follow: &mut (
             impl tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, tonic::Status>>
             + Unpin
         ),
    count: usize,
) -> Vec<follow_deployment_logs_response::Frame> {
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        frames.push(next_frame(follow).await.expect("an open follow"));
    }
    frames
}

/// The next `count` events of a manager follow's public stream.
async fn next_events(
    events: &mut (
             impl tokio_stream::Stream<Item = Result<DeploymentLogFollowEvent, tonic::Status>> + Unpin
         ),
    count: usize,
) -> Vec<DeploymentLogFollowEvent> {
    let mut received = Vec::with_capacity(count);
    for _ in 0..count {
        let event = tokio::time::timeout(PROMPT, events.next())
            .await
            .expect("event arrives promptly")
            .expect("an open follow")
            .expect("a follow event");
        received.push(event);
    }
    received
}

/// The events of `source` among `events`, in order, each as `<source> <kind> [detail]`.
fn of_source(events: &[DeploymentLogFollowEvent], source: &RuntimeLogSource) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            DeploymentLogFollowEvent::Available(available) if available.name == source.name => {
                Some(format!("{} available {}", available.name, available.label))
            }
            DeploymentLogFollowEvent::Unavailable(unavailable)
                if unavailable.name == source.name =>
            {
                Some(format!("{} unavailable", unavailable.name))
            }
            DeploymentLogFollowEvent::Ended(ended) if ended.name == source.name => {
                Some(format!("{} ended", ended.name))
            }
            DeploymentLogFollowEvent::Entry(entry) if entry.source.name == source.name => {
                Some(format!(
                    "{} entry {}",
                    entry.source.name,
                    String::from_utf8_lossy(&entry.content)
                ))
            }
            _ => None,
        })
        .collect()
}

/// A runtime stream the test feeds entry by entry, and ends by dropping the sender.
fn fed_stream() -> (
    tokio::sync::mpsc::UnboundedSender<Result<RuntimeLogEvent, RuntimeBackendError>>,
    RuntimeLogStream,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        sender,
        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(
            receiver,
        )),
    )
}

/// Feeds one model-source line at `second`.
fn feed(
    stream: &tokio::sync::mpsc::UnboundedSender<Result<RuntimeLogEvent, RuntimeBackendError>>,
    second: u64,
    content: &str,
) {
    stream
        .send(Ok(RuntimeLogEvent::Entry(entry(
            &model_source(),
            second,
            content.as_bytes().to_vec(),
        ))))
        .expect("a fed stream");
}

fn opened_frame(
    source: &RuntimeLogSource,
    unavailable: Option<DeploymentLogUnavailableReason>,
) -> follow_deployment_logs_response::Frame {
    follow_deployment_logs_response::Frame::Opened(DeploymentLogFollowOpened {
        sources: vec![opening(source, unavailable)],
    })
}

fn source_opened_frame(
    source: &RuntimeLogSource,
    unavailable: Option<DeploymentLogUnavailableReason>,
) -> follow_deployment_logs_response::Frame {
    follow_deployment_logs_response::Frame::SourceOpened(opening(source, unavailable))
}

fn opening(
    source: &RuntimeLogSource,
    unavailable: Option<DeploymentLogUnavailableReason>,
) -> DeploymentLogSourceOpening {
    DeploymentLogSourceOpening {
        source: Some(DeploymentLogSource {
            name: source.name.clone(),
            label: source.label.clone(),
        }),
        unavailable: unavailable.map(|reason| reason as i32),
    }
}

/// The frame of one model-source line at `second`.
fn entry_frame(second: u64, content: &str) -> follow_deployment_logs_response::Frame {
    follow_deployment_logs_response::Frame::Entry(
        dotcore_proto::dotcore::deployment::private::v1::DeploymentLogSourceEntry {
            source_name: "model".to_owned(),
            entry: Some(super::private_entry(
                entry(&model_source(), second, content.as_bytes().to_vec()),
                &ProtectedLogValues::default(),
            )),
        },
    )
}

fn ended_frame(source: &RuntimeLogSource) -> follow_deployment_logs_response::Frame {
    follow_deployment_logs_response::Frame::SourceEnded(DeploymentLogSourceEnded {
        source_name: source.name.clone(),
    })
}

/// Loopback TCP relay to the worker that counts the connections the manager opens.
struct CountingProxy {
    address: SocketAddr,
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingProxy {
    async fn start(target: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy listener");
        let address = listener.local_addr().expect("proxy address");
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&connections);
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });
        Self {
            address,
            connections,
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The loopback address of `reserved`, which nothing listens on.
fn closed_address(reserved: &ReservedPort) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], reserved.port()))
}

fn entry(source: &RuntimeLogSource, second: u64, content: Vec<u8>) -> RuntimeLogEntry {
    RuntimeLogEntry {
        source: source.clone(),
        timestamp: UNIX_EPOCH + Duration::from_secs(second),
        stream: RuntimeLogStreamKind::Stdout,
        content,
        truncated: false,
    }
}

fn model_source() -> RuntimeLogSource {
    RuntimeLogSource {
        name: "model".to_owned(),
        label: "Model".to_owned(),
    }
}

fn engine_source() -> RuntimeLogSource {
    RuntimeLogSource {
        name: "engine".to_owned(),
        label: "Engine".to_owned(),
    }
}

fn manager_session() -> ManagerSession {
    ManagerSession::parse("a".repeat(32)).expect("manager session")
}

fn node_incarnation() -> NodeIncarnation {
    NodeIncarnation::parse("1".repeat(32)).expect("worker incarnation")
}
