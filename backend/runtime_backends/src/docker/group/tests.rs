//! Docker workload-group lifecycle characterization tests.

use std::{
    error::Error as _,
    future::ready,
    net::{Ipv4Addr, SocketAddrV4},
    num::NonZeroU16,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use dotcore_domain::{SiteNodeId, test_support::host_resolver::FakeHostResolver};
use dotcore_runtime::{
    AssignedWorkloadEndpoint, ExecutionTarget, RuntimeBackendId, RuntimeGroupLogFollowQuery,
    RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceSelection, WorkloadCorrelation,
    WorkloadDependencyCondition, WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupStaleReason,
    WorkloadPort, WorkloadPublicationRequest, WorkloadStorageAttachment, WorkloadStorageKind,
    WorkloadUnitDependency, WorkloadUnitLaunch, WorkloadUnitObservation, WorkloadUnitState,
};

use super::{
    super::{
        adapter::{DockerAcceleratorTransport, DockerRuntimeConfig},
        client::{
            ClientError, ContainerInspection, DockerClient, DockerContainer, DockerGroupExecution,
            DockerLogOutput, DockerLogRequest, DockerLogStream, DockerNetwork,
            DockerNetworkSummary, GroupContainerInspection, GroupContainerOwnership,
            GroupNetworkAttachment,
        },
        ownership::{
            LABEL_CORRELATION, LABEL_EXECUTION_TARGET, LABEL_GENERATION, LABEL_LAUNCH_FINGERPRINT,
            LABEL_MANAGED, LABEL_PORTS, LABEL_SCHEMA, LABEL_SITE, LABEL_WORKLOAD, OWNERSHIP_SCHEMA,
        },
        request::{
            DockerCreateRequest, DockerDeviceRequest, DockerGroupContainerRequest,
            DockerGroupNetworkRequest, DockerGroupPort,
        },
        storage::materialize_storage,
    },
    *,
};

#[tokio::test]
async fn two_service_group_uses_private_dns_dependency_order_one_publication_and_full_teardown() {
    let root = dotcore_domain::test_support::canonical_temp_dir()
        .join(format!("dotcore-group-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("Application site-data root");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let requests = client.requests.clone();
    let adapter = DockerAdapter::new(
        client,
        DockerRuntimeConfig {
            site_id: "site-one".to_owned(),
            execution_target_id: "application-target-one".to_owned(),
            approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
            application_data_root: root.clone(),
            workload_cache_root: root.clone(),
            accelerator_transport: DockerAcceleratorTransport::Cdi,
            source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
        },
    );
    let group = group();

    let observation = adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    assert!(matches!(observation, WorkloadGroupObservation::Observed(_)));
    let creation_calls = calls.lock().expect("calls").clone();
    assert_eq!(
        creation_calls[..5],
        [
            "network:dotcore_site-one_group_apps-01",
            "create:dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "create:dotcore_site-one_group_apps-01_unit_web",
            "start:container-dotcore_site-one_group_apps-01_unit_web"
        ]
    );
    {
        let requests = requests.lock().expect("requests");
        let cache = requests
            .get("dotcore_site-one_group_apps-01_unit_cache")
            .expect("cache request");
        let web = requests
            .get("dotcore_site-one_group_apps-01_unit_web")
            .expect("web request");
        assert_eq!(cache.aliases, ["cache"]);
        assert_eq!(cache.entrypoint, ["/bin/server"]);
        assert_eq!(cache.argv, Vec::<String>::new());
        assert_eq!(web.aliases, ["web"]);
        assert_eq!(web.entrypoint, ["/bin/server"]);
        assert_eq!(web.argv, Vec::<String>::new());
        assert_eq!(
            cache.mounts[0].source,
            root.join("deployment/apps-01").join("cache-data")
        );
        assert!(cache.ports.iter().all(|port| port.publication.is_none()));
        assert_eq!(
            web.ports
                .iter()
                .filter_map(|port| port.publication)
                .collect::<Vec<_>>(),
            vec![SocketAddrV4::new(Ipv4Addr::new(10, 20, 30, 40), 40_000)]
        );
    }

    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove group");
    let calls = calls.lock().expect("calls");
    assert_eq!(
        &calls[calls.len() - 5..],
        [
            "stop:container-dotcore_site-one_group_apps-01_unit_web",
            "remove:container-dotcore_site-one_group_apps-01_unit_web",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
            "remove:container-dotcore_site-one_group_apps-01_unit_cache",
            "remove-network:network-dotcore_site-one_group_apps-01"
        ]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn shared_daemon_keeps_same_owner_targets_and_model_resources_isolated_through_recovery() {
    let root = test_root("shared-daemon-target-isolation");
    let client = GroupClient::default();
    let first = adapter_for_target(client.clone(), root.clone(), "application-target-one");
    let second = adapter_for_target(client.clone(), root.clone(), "application-target-two");
    let first_group = group_for_target("apps-01", "deployment-one", "application-target-one");
    let second_group = group_for_target("apps-02", "deployment-two", "application-target-two");
    let model = model_container("model-target");
    client
        .extra_containers
        .lock()
        .expect("extra containers")
        .push(model.clone());

    first
        .create_workload_group(&first_group)
        .await
        .expect("create first exact-target group");
    second
        .create_workload_group(&second_group)
        .await
        .expect("create second exact-target group");
    assert_eq!(
        first
            .discover_workload_groups()
            .await
            .expect("first discovery")
            .len(),
        1
    );
    assert_eq!(
        second
            .discover_workload_groups()
            .await
            .expect("second discovery")
            .len(),
        1
    );
    assert!(second.inspect_workload_group(&first_group).await.is_err());

    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
    for (group_id, content) in [("apps-01", "first-target"), ("apps-02", "second-target")] {
        client.logs.lock().expect("logs").insert(
            format!("container-dotcore_site-one_group_{group_id}_unit_web"),
            vec![Ok(DockerLogOutput::stdout(
                timestamp,
                format!("{content}\n").into_bytes(),
            ))],
        );
    }
    let sources = RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
        name: "web".to_owned(),
        label: "Public web".to_owned(),
    }])
    .expect("selected source");
    let query = RuntimeLogQuery::new(std::num::NonZeroU16::new(20).expect("nonzero"), None)
        .expect("bounded query");
    let first_logs = first
        .read_workload_group_logs(&first_group, &sources, &query)
        .await
        .expect("first target logs");
    let second_logs = second
        .read_workload_group_logs(&second_group, &sources, &query)
        .await
        .expect("second target logs");
    assert_eq!(
        first_logs[0]
            .result
            .as_ref()
            .expect("first history")
            .entries[0]
            .content,
        b"first-target"
    );
    assert_eq!(
        second_logs[0]
            .result
            .as_ref()
            .expect("second history")
            .entries[0]
            .content,
        b"second-target"
    );
    assert!(
        second
            .read_workload_group_logs(&first_group, &sources, &query)
            .await
            .is_err()
    );

    first
        .stop_workload_group(&first_group)
        .await
        .expect("pause first target");
    second
        .stop_workload_group(&second_group)
        .await
        .expect("pause second target");
    let recovered_first =
        adapter_for_target(client.clone(), root.clone(), "application-target-one");
    let recovered_second =
        adapter_for_target(client.clone(), root.clone(), "application-target-two");
    assert!(matches!(
        recovered_first
            .inspect_workload_group(&first_group)
            .await
            .expect("reconstruct first target"),
        WorkloadGroupObservation::Observed(_)
    ));
    assert!(matches!(
        recovered_second
            .inspect_workload_group(&second_group)
            .await
            .expect("reconstruct second target"),
        WorkloadGroupObservation::Observed(_)
    ));
    recovered_first
        .start_workload_group(&first_group)
        .await
        .expect("resume first target");
    recovered_second
        .start_workload_group(&second_group)
        .await
        .expect("resume second target");

    recovered_first
        .remove_workload_group(&first_group)
        .await
        .expect("clean first target");
    let remaining = client
        .list_managed_containers()
        .await
        .expect("remaining resources");
    assert!(remaining.iter().any(|container| {
        container
            .labels
            .get(LABEL_EXECUTION_TARGET)
            .map(String::as_str)
            == Some("application-target-two")
    }));
    assert!(remaining.contains(&model));
    recovered_second
        .remove_workload_group(&second_group)
        .await
        .expect("clean second target");
    assert_eq!(
        client
            .list_managed_containers()
            .await
            .expect("foreign model retained"),
        vec![model]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stable_identity_cleanup_removes_only_exact_group_resources_and_preserves_data() {
    let root = test_root("stable-cleanup");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    let data = root.join("deployment/apps-01").join("cache-data");
    std::fs::create_dir_all(&data).expect("persistent application data");
    std::fs::write(data.join("marker"), "retained").expect("persistent data marker");
    client
        .extra_containers
        .lock()
        .expect("extra containers")
        .push(DockerContainer {
            id: "unrelated-malformed".to_owned(),
            name: "unrelated-malformed".to_owned(),
            name_is_unambiguous: false,
            labels: BTreeMap::from([
                (
                    super::super::ownership::LABEL_SITE.to_owned(),
                    "site-one".to_owned(),
                ),
                (LABEL_GROUP.to_owned(), "unrelated-group".to_owned()),
            ]),
        });

    adapter
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("stable identity cleanup");
    adapter
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("repeated stable identity cleanup");

    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    assert_eq!(
        std::fs::read_to_string(data.join("marker")).expect("retained data marker"),
        "retained"
    );
    assert_eq!(
        client
            .extra_containers
            .lock()
            .expect("extra containers")
            .len(),
        1
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stable_identity_cleanup_removes_network_only_residue_with_a_fresh_adapter() {
    let root = test_root("stable-network-residue");
    let client = GroupClient::default();
    let group = group();
    test_adapter(client.clone(), root.clone())
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    client.requests.lock().expect("requests").clear();
    let fresh = test_adapter(client.clone(), root.clone());

    fresh
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("network-only stable identity cleanup");

    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stable_identity_cleanup_clears_cached_group_after_external_removal() {
    let root = test_root("stable-external-removal");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    client.requests.lock().expect("requests").clear();
    *client.network.lock().expect("network") = None;

    adapter
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("cleanup after external removal");

    assert!(
        adapter
            .known_groups()
            .lock()
            .expect("known groups")
            .is_empty()
    );
    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stable_identity_cleanup_fails_closed_for_owner_generation_and_duplicate_conflicts() {
    for conflict in ["owner", "generation", "duplicate"] {
        let root = test_root(&format!("stable-{conflict}-conflict"));
        let client = GroupClient::default();
        let adapter = test_adapter(client.clone(), root.clone());
        let group = group();
        adapter
            .create_workload_group(&group)
            .await
            .expect("create exact group");
        let owner = if conflict == "owner" {
            "another-owner"
        } else {
            &group.correlation.owner_id
        };
        if conflict != "owner" {
            let request = client
                .requests
                .lock()
                .expect("requests")
                .values()
                .next()
                .expect("group request")
                .clone();
            let mut labels = request.labels;
            let (id, name) = if conflict == "generation" {
                labels.insert(
                    super::super::ownership::LABEL_GENERATION.to_owned(),
                    "2".to_owned(),
                );
                labels.insert(LABEL_UNIT.to_owned(), "worker".to_owned());
                (
                    "conflicting-generation".to_owned(),
                    "dotcore_site-one_group_apps-01_unit_worker".to_owned(),
                )
            } else {
                ("duplicate-unit".to_owned(), request.name)
            };
            client
                .extra_containers
                .lock()
                .expect("extra containers")
                .push(DockerContainer {
                    id,
                    name,
                    name_is_unambiguous: true,
                    labels,
                });
        }

        let before = client.requests.lock().expect("requests").len();
        assert!(
            adapter
                .ensure_workload_group_absent(&group.group_id, owner)
                .await
                .is_err(),
            "{conflict} conflict must fail closed"
        );
        assert_eq!(client.requests.lock().expect("requests").len(), before);
        assert!(client.network.lock().expect("network").is_some());
        std::fs::remove_dir_all(root).expect("remove application site-data root");
    }
}

#[tokio::test]
async fn stable_identity_cleanup_rejects_foreign_network_attachments_without_deletion() {
    let root = test_root("stable-foreign-attachment");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    client
        .extra_network_attachments
        .lock()
        .expect("attachments")
        .insert("foreign-container".to_owned());

    assert!(
        adapter
            .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
            .await
            .is_err()
    );
    assert_eq!(client.requests.lock().expect("requests").len(), 2);
    assert!(client.network.lock().expect("network").is_some());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stable_identity_cleanup_rejects_another_targets_group_without_deletion() {
    let root = test_root("stable-foreign-target");
    let client = GroupClient::default();
    let owner = adapter_for_target(client.clone(), root.clone(), "application-target-one");
    let other = adapter_for_target(client.clone(), root.clone(), "application-target-two");
    let group = group();
    owner
        .create_workload_group(&group)
        .await
        .expect("create exact group");

    assert!(
        other
            .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
            .await
            .is_err()
    );
    assert_eq!(client.requests.lock().expect("requests").len(), 2);
    assert!(client.network.lock().expect("network").is_some());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stable_identity_cleanup_retries_after_partial_container_removal() {
    let root = test_root("stable-removal-retry");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    *client.fail_remove_unit.lock().expect("remove failure") = Some("web".to_owned());

    assert!(
        adapter
            .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
            .await
            .is_err()
    );
    assert_eq!(client.requests.lock().expect("requests").len(), 1);
    assert!(client.network.lock().expect("network").is_some());
    *client.fail_remove_unit.lock().expect("remove failure") = None;

    adapter
        .ensure_workload_group_absent(&group.group_id, &group.correlation.owner_id)
        .await
        .expect("stable cleanup retry");
    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_logs_preserve_public_sources_lookahead_and_independent_resume_positions() {
    use futures_util::StreamExt as _;

    let root = test_root("group-logs");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
    for unit in ["cache", "web"] {
        client.logs.lock().expect("logs").insert(
            format!("container-dotcore_site-one_group_apps-01_unit_{unit}"),
            vec![Ok(DockerLogOutput::stdout(
                timestamp,
                format!("{unit}-line\n").into_bytes(),
            ))],
        );
    }
    let sources = RuntimeLogSourceSelection::new(vec![
        RuntimeLogSource {
            name: "cache".to_owned(),
            label: "Public cache".to_owned(),
        },
        RuntimeLogSource {
            name: "web".to_owned(),
            label: "Public web".to_owned(),
        },
    ])
    .expect("selected sources");
    let history_query =
        RuntimeLogQuery::new(std::num::NonZeroU16::new(200).expect("non-zero"), None)
            .expect("bounded history");

    let histories = adapter
        .read_workload_group_logs(&group, &sources, &history_query)
        .await
        .expect("group history");

    assert_eq!(histories.len(), 2);
    for (history, expected_name, expected_label, expected_content) in [
        (
            &histories[0],
            "cache",
            "Public cache",
            b"cache-line".as_slice(),
        ),
        (&histories[1], "web", "Public web", b"web-line".as_slice()),
    ] {
        assert_eq!(history.source.name, expected_name);
        assert_eq!(history.source.label, expected_label);
        let entries = &history.result.as_ref().expect("source history").entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, history.source);
        assert_eq!(entries[0].content, expected_content);
    }
    assert!(
        client
            .log_requests
            .lock()
            .expect("log requests")
            .iter()
            .all(|request| request.tail == Some(201))
    );

    for unit in ["cache", "web"] {
        client.logs.lock().expect("logs").insert(
            format!("container-dotcore_site-one_group_apps-01_unit_{unit}"),
            vec![Ok(DockerLogOutput::stdout(
                timestamp,
                format!("{unit}-follow\n").into_bytes(),
            ))],
        );
    }
    let cache_resume = timestamp - std::time::Duration::from_nanos(2);
    let web_resume = timestamp;
    let follow_query = RuntimeGroupLogFollowQuery::new(
        &sources,
        BTreeMap::from([
            ("cache".to_owned(), cache_resume),
            ("web".to_owned(), web_resume),
        ]),
        tokio_util::sync::CancellationToken::new(),
    )
    .expect("group follow");
    let streams = adapter
        .follow_workload_group_logs(&group, &sources, follow_query)
        .await
        .expect("group streams");
    assert_eq!(streams.len(), 2);
    for stream in streams {
        let source = stream.source.clone();
        let events = stream
            .result
            .expect("source stream")
            .collect::<Vec<_>>()
            .await;
        // The web line is at the resume position, so only the cache line is delivered.
        if source.name == "web" {
            assert!(events.is_empty(), "web events: {events:?}");
            continue;
        }
        let dotcore_runtime::RuntimeLogEvent::Entry(entry) =
            events[0].as_ref().expect("follow entry")
        else {
            panic!("entry event")
        };
        assert_eq!(entry.source, source);
        assert_eq!(entry.content, b"cache-follow");
    }
    let requests = client.log_requests.lock().expect("log requests");
    let follow_requests = &requests[2..];
    assert_eq!(follow_requests.len(), 2);
    // Docker is asked for all output since the resume position's whole second; the exact position
    // is applied to the returned entries.
    let floored = std::time::UNIX_EPOCH + std::time::Duration::from_secs(9);
    let web_floored = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
    assert_eq!(follow_requests[0].since, Some(floored));
    assert_eq!(follow_requests[1].since, Some(web_floored));
    assert!(follow_requests.iter().all(|request| request.tail.is_none()));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

/// Follows the group's `cache` source over the given Docker records and returns what the follow
/// asked Docker for with the entries it delivered as `(content, seconds since epoch, nanos)`.
async fn follow_cache_records(
    name: &str,
    records: Vec<DockerLogOutput>,
    resume: Option<std::time::SystemTime>,
) -> (DockerLogRequest, Vec<(Vec<u8>, std::time::SystemTime)>) {
    use futures_util::StreamExt as _;

    let root = test_root(name);
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    client.logs.lock().expect("logs").insert(
        "container-dotcore_site-one_group_apps-01_unit_cache".to_owned(),
        records.into_iter().map(Ok).collect(),
    );
    let sources = RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
        name: "cache".to_owned(),
        label: "Cache".to_owned(),
    }])
    .expect("source");
    let resume_positions = resume
        .map(|resume| BTreeMap::from([("cache".to_owned(), resume)]))
        .unwrap_or_default();
    let query = RuntimeGroupLogFollowQuery::new(
        &sources,
        resume_positions,
        tokio_util::sync::CancellationToken::new(),
    )
    .expect("group follow");
    let mut streams = adapter
        .follow_workload_group_logs(&group, &sources, query)
        .await
        .expect("group streams");
    let events = streams
        .remove(0)
        .result
        .expect("source stream")
        .collect::<Vec<_>>()
        .await;
    let entries = events
        .into_iter()
        .map(|event| match event.expect("follow event") {
            dotcore_runtime::RuntimeLogEvent::Entry(entry) => (entry.content, entry.timestamp),
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    let request = client
        .log_requests
        .lock()
        .expect("log requests")
        .last()
        .expect("follow request")
        .clone();
    std::fs::remove_dir_all(root).expect("remove application site-data root");
    (request, entries)
}

#[tokio::test]
async fn group_follow_resume_does_not_merge_a_replayed_partial_line_into_the_next_run() {
    let at = |millis| std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis);
    let (request, entries) = follow_cache_records(
        "group-follow-partial",
        vec![
            DockerLogOutput::stdout(at(10_100), b"progress...".to_vec()),
            DockerLogOutput::stdout(at(10_900), b"started\n".to_vec()),
        ],
        Some(at(10_100)),
    )
    .await;

    assert_eq!(request.since, Some(at(10_000)));
    assert_eq!(entries, vec![(b"started".to_vec(), at(10_900))]);
}

#[tokio::test]
async fn group_follow_flushes_both_trailing_partial_lines() {
    let at = |millis| std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis);
    let (_, entries) = follow_cache_records(
        "group-follow-trailing",
        vec![
            DockerLogOutput::stdout(at(10_100), b"out-tail".to_vec()),
            DockerLogOutput::stderr(at(10_200), b"err-tail".to_vec()),
        ],
        Some(at(10_000)),
    )
    .await;

    assert_eq!(
        entries,
        vec![
            (b"out-tail".to_vec(), at(10_100)),
            (b"err-tail".to_vec(), at(10_200)),
        ]
    );
}

#[tokio::test]
async fn group_follow_without_a_resume_position_replays_a_bounded_tail_from_the_first_line() {
    let at = |millis| std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis);
    let (request, entries) = follow_cache_records(
        "group-follow-bounded",
        vec![DockerLogOutput::stdout(at(10_100), b"first\n".to_vec())],
        None,
    )
    .await;

    assert_eq!(request.since, None);
    assert_eq!(request.tail, Some(dotcore_runtime::MAX_RUNTIME_LOG_ENTRIES));
    assert_eq!(entries, vec![(b"first".to_vec(), at(10_100))]);
}

#[tokio::test(start_paused = true)]
async fn group_history_has_one_deadline_for_the_complete_operation() {
    let root = test_root("group-log-history-deadline");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    client
        .pending_logs
        .lock()
        .expect("pending logs")
        .insert("container-dotcore_site-one_group_apps-01_unit_cache".to_owned());
    let sources = RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
        name: "cache".to_owned(),
        label: "Cache".to_owned(),
    }])
    .expect("source");
    let query = RuntimeLogQuery::new(std::num::NonZeroU16::new(1).expect("non-zero"), None)
        .expect("history query");

    let error = adapter
        .read_workload_group_logs(&group, &sources, &query)
        .await
        .expect_err("pending complete history must time out");

    assert_eq!(
        error.to_string(),
        "Docker runtime failure: Docker group log history acquisition timed out"
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_logs_before_creation_read_as_not_created() {
    let root = test_root("group-logs-before-creation");
    let adapter = test_adapter(GroupClient::default(), root.clone());
    let group = group();
    let sources = RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
        name: "web".to_owned(),
        label: "Public web".to_owned(),
    }])
    .expect("source");
    let query = RuntimeLogQuery::new(std::num::NonZeroU16::new(1).expect("non-zero"), None)
        .expect("history query");

    let history = adapter
        .read_workload_group_logs(&group, &sources, &query)
        .await
        .expect("history before creation");
    let follow = adapter
        .follow_workload_group_logs(
            &group,
            &sources,
            RuntimeGroupLogFollowQuery::new(
                &sources,
                std::collections::BTreeMap::new(),
                tokio_util::sync::CancellationToken::new(),
            )
            .expect("follow query"),
        )
        .await
        .expect("follow before creation");

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].source.name, "web");
    assert!(matches!(
        history[0].result,
        Err(RuntimeBackendError::WorkloadNotCreated)
    ));
    assert_eq!(follow.len(), 1);
    assert!(matches!(
        follow[0].result,
        Err(RuntimeBackendError::WorkloadNotCreated)
    ));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_history_keeps_one_removed_source_from_denying_a_healthy_source() {
    let root = test_root("group-log-source-isolation");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
    client.logs.lock().expect("logs").insert(
        "container-dotcore_site-one_group_apps-01_unit_cache".to_owned(),
        vec![Ok(DockerLogOutput::stdout(
            timestamp,
            b"healthy\n".to_vec(),
        ))],
    );
    client
        .requests
        .lock()
        .expect("requests")
        .remove("dotcore_site-one_group_apps-01_unit_web");
    let sources = RuntimeLogSourceSelection::new(vec![
        RuntimeLogSource {
            name: "cache".to_owned(),
            label: "Cache".to_owned(),
        },
        RuntimeLogSource {
            name: "web".to_owned(),
            label: "Web".to_owned(),
        },
    ])
    .expect("sources");
    let query = RuntimeLogQuery::new(std::num::NonZeroU16::new(1).expect("non-zero"), None)
        .expect("history query");

    let histories = adapter
        .read_workload_group_logs(&group, &sources, &query)
        .await
        .expect("source-specific histories");

    assert_eq!(
        histories[0]
            .result
            .as_ref()
            .expect("healthy cache history")
            .entries[0]
            .content,
        b"healthy"
    );
    assert!(histories[1].result.is_err());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_follow_isolates_partial_admission_and_releases_every_opened_permit() {
    let root = test_root("group-log-partial-admission");
    let client = GroupClient::default();
    let permits = crate::docker::configuration::DockerLogSourcePermits::new();
    let adapter = DockerAdapter::new(
        client,
        DockerRuntimeConfig {
            source_stream_permits: permits.clone(),
            ..test_config(&root)
        },
    );
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    let held = (0..31)
        .map(|_| permits.try_acquire().expect("reserved source permit"))
        .collect::<Vec<_>>();
    let sources = RuntimeLogSourceSelection::new(vec![
        RuntimeLogSource {
            name: "cache".to_owned(),
            label: "Cache".to_owned(),
        },
        RuntimeLogSource {
            name: "web".to_owned(),
            label: "Web".to_owned(),
        },
    ])
    .expect("sources");
    let query = RuntimeGroupLogFollowQuery::new(
        &sources,
        BTreeMap::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .expect("follow query");

    let streams = adapter
        .follow_workload_group_logs(&group, &sources, query)
        .await
        .expect("source-isolated admission");

    assert!(streams[0].result.is_ok());
    assert!(matches!(
        streams[1].result,
        Err(RuntimeBackendError::LogSourceLimitReached)
    ));
    drop(streams);
    drop(held);
    let all = (0..32)
        .map(|_| permits.try_acquire().expect("released source permit"))
        .collect::<Vec<_>>();
    assert!(matches!(
        permits.try_acquire(),
        Err(RuntimeBackendError::LogSourceLimitReached)
    ));
    drop(all);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_follow_cancellation_during_setup_releases_partially_opened_sources() {
    let root = test_root("group-log-setup-cancellation");
    let client = GroupClient::default();
    let permits = crate::docker::configuration::DockerLogSourcePermits::new();
    let adapter = DockerAdapter::new(
        client.clone(),
        DockerRuntimeConfig {
            source_stream_permits: permits.clone(),
            ..test_config(&root)
        },
    );
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    client
        .pending_ownership
        .lock()
        .expect("pending ownership")
        .insert("container-dotcore_site-one_group_apps-01_unit_web".to_owned());
    let sources = RuntimeLogSourceSelection::new(vec![
        RuntimeLogSource {
            name: "cache".to_owned(),
            label: "Cache".to_owned(),
        },
        RuntimeLogSource {
            name: "web".to_owned(),
            label: "Web".to_owned(),
        },
    ])
    .expect("sources");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let query = RuntimeGroupLogFollowQuery::new(&sources, BTreeMap::new(), cancellation.clone())
        .expect("follow query");
    let follow = adapter.follow_workload_group_logs(&group, &sources, query);
    tokio::pin!(follow);
    tokio::select! {
        result = &mut follow => panic!("setup unexpectedly completed: {}", result.is_ok()),
        _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
    }

    cancellation.cancel();
    let error = match follow.await {
        Ok(_) => panic!("cancelled setup must not return streams"),
        Err(error) => error,
    };
    assert!(error.is_log_cancelled());
    let all = (0..32)
        .map(|_| permits.try_acquire().expect("released setup permit"))
        .collect::<Vec<_>>();
    assert_eq!(all.len(), 32);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_follow_cancellation_stops_pending_poll_and_releases_its_permit() {
    use futures_util::StreamExt as _;

    let root = test_root("group-log-cancellation");
    let client = GroupClient::default();
    let permits = crate::docker::configuration::DockerLogSourcePermits::new();
    let adapter = DockerAdapter::new(
        client.clone(),
        DockerRuntimeConfig {
            source_stream_permits: permits.clone(),
            ..test_config(&root)
        },
    );
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create exact group");
    let container_id = "container-dotcore_site-one_group_apps-01_unit_cache".to_owned();
    client
        .pending_logs
        .lock()
        .expect("pending logs")
        .insert(container_id);
    let sources = RuntimeLogSourceSelection::new(vec![RuntimeLogSource {
        name: "cache".to_owned(),
        label: "Cache".to_owned(),
    }])
    .expect("source");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let query = RuntimeGroupLogFollowQuery::new(&sources, BTreeMap::new(), cancellation.clone())
        .expect("follow query");
    let mut stream = adapter
        .follow_workload_group_logs(&group, &sources, query)
        .await
        .expect("group streams")
        .remove(0)
        .result
        .expect("source stream");

    cancellation.cancel();
    assert!(stream.next().await.is_none());
    drop(stream);
    let all = (0..32)
        .map(|_| permits.try_acquire().expect("released cancellation permit"))
        .collect::<Vec<_>>();
    assert_eq!(all.len(), 32);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn retained_complete_group_is_observed_after_its_image_becomes_unavailable() {
    let root = test_root("retained-complete-missing-image");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create complete group");
    client
        .missing_images
        .lock()
        .expect("missing images")
        .insert(
            group.units[&WorkloadUnitName("web".to_owned())]
                .image
                .clone(),
        );
    let calls_before_observation = calls.lock().expect("calls").len();

    let observation = adapter
        .create_workload_group(&group)
        .await
        .expect("complete retained group remains observable");

    assert!(matches!(observation, WorkloadGroupObservation::Observed(_)));
    assert_eq!(calls.lock().expect("calls").len(), calls_before_observation);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_creation_accepts_tag_only_and_combined_images() {
    for (case, image) in [
        ("tag-only", "example.test/workload:dev".to_owned()),
        (
            "combined",
            format!("example.test/workload:release@sha256:{}", "a".repeat(64)),
        ),
    ] {
        let root = test_root(case);
        let client = GroupClient::default();
        let inspections = client.image_inspections.clone();
        let mut group = group();
        for unit in group.units.values_mut() {
            unit.image.clone_from(&image);
        }

        let observation = test_adapter(client, root.clone())
            .create_workload_group(&group)
            .await
            .expect("create group with canonical image reference");

        assert!(matches!(observation, WorkloadGroupObservation::Observed(_)));
        assert_eq!(
            *inspections.lock().expect("image inspections"),
            vec![image.clone(), image]
        );
        std::fs::remove_dir_all(root).expect("remove application site-data root");
    }
}

#[tokio::test]
async fn malformed_group_image_is_rejected_before_inspection_or_mutation() {
    let root = test_root("malformed-image");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let inspections = client.image_inspections.clone();
    let mut group = group();
    group
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit")
        .image = "Example.test/workload:dev".to_owned();

    assert!(
        test_adapter(client, root.clone())
            .create_workload_group(&group)
            .await
            .is_err()
    );
    assert!(calls.lock().expect("calls").is_empty());
    assert!(inspections.lock().expect("image inspections").is_empty());
    assert!(!root.join("deployment/apps-01").exists());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_creation_classifies_a_race_lost_during_container_creation_as_image_missing() {
    let root = test_root("race-missing-image");
    let client = GroupClient::default();
    let group = group();
    client
        .create_unit_not_found
        .lock()
        .expect("create not-found")
        .replace("web".to_owned());
    let inspections = client.image_inspections.clone();

    let error = test_adapter(client.clone(), root.clone())
        .create_workload_group(&group)
        .await
        .expect_err("create race must fail");

    let RuntimeBackendError::ImageMissing(source) = &error else {
        panic!("race must classify as ImageMissing: {error}");
    };
    assert!(!error.to_string().contains("no such image"));
    assert!(source.chain().any(|cause| cause.is::<ClientError>()));
    assert_eq!(
        *inspections.lock().expect("image inspections"),
        group
            .units
            .values()
            .map(|unit| unit.image.clone())
            .collect::<Vec<_>>()
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_creation_keeps_non_image_container_creation_failures_generic() {
    let root = test_root("race-generic-create-failure");
    let client = GroupClient::default();
    let group = group();
    client
        .fail_create_unit
        .lock()
        .expect("create failure")
        .replace("web".to_owned());

    let error = test_adapter(client, root.clone())
        .create_workload_group(&group)
        .await
        .expect_err("create failure must fail");

    assert!(matches!(error, RuntimeBackendError::Docker(_)));
    assert!(!error.is_local_image_missing());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_creation_reports_a_missing_local_image_without_mutating_docker() {
    let root = test_root("missing-local-image");
    let client = GroupClient::default();
    let group = group();
    client
        .missing_images
        .lock()
        .expect("missing images")
        .insert(
            group.units[&WorkloadUnitName("cache".to_owned())]
                .image
                .clone(),
        );
    let calls = client.calls.clone();
    let inspections = client.image_inspections.clone();
    let expected_images = group
        .units
        .values()
        .map(|unit| unit.image.clone())
        .collect::<Vec<_>>();
    let adapter = test_adapter(client, root.clone());

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("missing local image must prevent group creation");

    assert!(matches!(error, RuntimeBackendError::ImageMissing(_)));
    assert!(calls.lock().expect("calls").is_empty());
    assert_eq!(
        *inspections.lock().expect("image inspections"),
        expected_images
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_containers_get_host_entries_resolved_on_the_hosting_node() {
    let root = test_root("host-aliases");
    let client = GroupClient::default();
    let requests = client.requests.clone();
    let resolver = Arc::new(
        FakeHostResolver::default()
            .with(
                "manager.example.ts.net",
                [std::net::Ipv4Addr::new(100, 64, 0, 7)],
            )
            .with("manager", [std::net::Ipv4Addr::new(127, 0, 0, 2)]),
    );
    let adapter = test_adapter(client, root.clone()).with_host_resolver(resolver.clone());
    let mut group = group();
    group.host_aliases = BTreeSet::from([alias("manager.example.ts.net"), alias("manager")]);

    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");

    let requests = requests.lock().expect("requests");
    for unit in ["cache", "web"] {
        let request = requests
            .get(&format!("dotcore_site-one_group_apps-01_unit_{unit}"))
            .expect("unit request");
        assert_eq!(
            request.extra_hosts,
            ["manager.example.ts.net:100.64.0.7"],
            "{unit}: a loopback result gets no entry"
        );
    }
    assert_eq!(resolver.lookups(), ["manager", "manager.example.ts.net"]);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn an_unresolvable_host_alias_fails_group_creation_before_mutating_docker() {
    let root = test_root("unresolvable-host-alias");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client, root.clone())
        .with_host_resolver(Arc::new(FakeHostResolver::default()));
    let mut group = group();
    group.host_aliases = BTreeSet::from([alias("manager.example.ts.net")]);

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("an unresolvable alias must fail creation");

    assert!(matches!(error, RuntimeBackendError::Docker(_)));
    assert!(!error.is_local_image_missing());
    assert_eq!(
        error.to_string(),
        "Docker runtime failure: Docker group host alias manager.example.ts.net cannot be \
         resolved on node node-one"
    );
    assert!(format!("{:#}", anyhow::Error::from(error)).ends_with(
        "could not resolve host name manager.example.ts.net; check that the network \
             providing it is up: manager.example.ts.net is unknown"
    ));
    assert!(calls.lock().expect("calls").is_empty());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_group_container_gets_its_host_alias_in_its_hosts_file() {
    let image = crate::docker::SITE_DATA_CLEANUP_IMAGE;
    assert!(docker(&["pull", image]).status.success(), "pull {image}");
    let client = super::super::client::LocalDockerClient::connect()
        .expect("Docker client")
        .negotiate_version()
        .await
        .expect("Docker API version");
    let platform = client.execution_platform().await.expect("Docker platform");
    let site = DockerSiteCleanup {
        site_id: format!("alias-{}", dotcore_domain::test_support::unique_suffix()),
        root: test_root("docker-host-alias"),
    };
    let mut config = test_config(&site.root);
    config.site_id = site.site_id.clone();
    let container = format!("dotcore_{}_group_apps-01_unit_web", site.site_id);
    let adapter = DockerAdapter::new(client, config).with_host_resolver(Arc::new(
        FakeHostResolver::default().with(
            "dotcore-alias.test",
            [std::net::Ipv4Addr::new(192, 0, 2, 10)],
        ),
    ));
    let mut group = group();
    group.required_platform = platform;
    group.publication.configured_host = std::net::Ipv4Addr::LOCALHOST;
    group.units = BTreeMap::from([(
        WorkloadUnitName("web".to_owned()),
        WorkloadUnitLaunch {
            image: image.to_owned(),
            // A shell PID 1 that traps SIGTERM stops at once; bare `sleep` ignores it until the
            // stop grace ends.
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "trap 'exit 0' TERM; sleep 300 & wait $!".to_owned(),
            ],
            storage: Vec::new(),
            ..unit("web", Vec::new())
        },
    )]);
    group.host_aliases = BTreeSet::from([alias("dotcore-alias.test")]);

    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let lookup = docker(&[
        "exec",
        &container,
        "grep",
        "dotcore-alias.test",
        "/etc/hosts",
    ]);

    assert!(lookup.status.success(), "host entry in {container}");
    assert_eq!(
        String::from_utf8(lookup.stdout)
            .expect("hosts file")
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["192.0.2.10", "dotcore-alias.test"]
    );
    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove group");
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_group_removal_removes_the_anonymous_volumes_of_its_containers() {
    let base = crate::docker::SITE_DATA_CLEANUP_IMAGE;
    assert!(docker(&["pull", base]).status.success(), "pull {base}");
    let suffix = dotcore_domain::test_support::unique_suffix();
    let image = format!("dotcore-test/anonymous-volume:{suffix}");
    let mut build = std::process::Command::new("docker")
        .args(["build", "--quiet", "--tag", &image, "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("docker build");
    std::io::Write::write_all(
        &mut build.stdin.take().expect("build input"),
        format!("FROM {base}\nVOLUME /data\n").as_bytes(),
    )
    .expect("write Dockerfile");
    assert!(
        build.wait().expect("docker build").success(),
        "build {image}"
    );
    let client = super::super::client::LocalDockerClient::connect()
        .expect("Docker client")
        .negotiate_version()
        .await
        .expect("Docker API version");
    let platform = client.execution_platform().await.expect("Docker platform");
    let site = DockerSiteCleanup {
        site_id: format!("volume-{suffix}"),
        root: test_root("docker-anonymous-volume"),
    };
    let mut config = test_config(&site.root);
    config.site_id = site.site_id.clone();
    let container = format!("dotcore_{}_group_apps-01_unit_web", site.site_id);
    let adapter = DockerAdapter::new(client, config);
    let mut group = group();
    group.required_platform = platform;
    group.publication.configured_host = std::net::Ipv4Addr::LOCALHOST;
    group.units = BTreeMap::from([(
        WorkloadUnitName("web".to_owned()),
        WorkloadUnitLaunch {
            image: image.clone(),
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "trap 'exit 0' TERM; sleep 300 & wait $!".to_owned(),
            ],
            storage: Vec::new(),
            ..unit("web", Vec::new())
        },
    )]);
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let mounts = docker(&[
        "container",
        "inspect",
        "--format",
        "{{range .Mounts}}{{if eq .Type \"volume\"}}{{.Name}} {{end}}{{end}}",
        &container,
    ]);
    let volumes = String::from_utf8(mounts.stdout)
        .expect("mounts")
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(volumes.len(), 1, "one anonymous volume for /data");

    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove group");

    assert!(
        !docker(&["volume", "inspect", &volumes[0]]).status.success(),
        "the anonymous volume {} goes with its container",
        volumes[0]
    );
    docker(&["image", "rm", &image]);
}

#[tokio::test]
async fn teardown_uses_reverse_dependency_order_not_reverse_lexical_order() {
    let root = test_root("reverse-dependency-order");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = reverse_lexical_group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");

    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove group");

    let calls = calls.lock().expect("calls");
    assert_eq!(
        &calls[calls.len() - 5..],
        [
            "stop:container-dotcore_site-one_group_apps-01_unit_alpha",
            "remove:container-dotcore_site-one_group_apps-01_unit_alpha",
            "stop:container-dotcore_site-one_group_apps-01_unit_zulu",
            "remove:container-dotcore_site-one_group_apps-01_unit_zulu",
            "remove-network:network-dotcore_site-one_group_apps-01",
        ]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_reconstructs_a_stale_group_for_observed_removal() {
    let root = test_root("restart-stale-removal");
    let client = GroupClient::default();
    let mut initial = group();
    initial
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit")
        .environment
        .insert("ROUTE_NAME".to_owned(), "initial-route".to_owned());
    initial
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit")
        .reconfigurable_environment
        .insert("ROUTE_NAME".to_owned());
    test_adapter(client.clone(), root.clone())
        .create_workload_group(&initial)
        .await
        .expect("create initial group");

    let mut renamed = initial.clone();
    renamed.correlation.reconcile_generation = 2;
    renamed
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit")
        .environment
        .insert("ROUTE_NAME".to_owned(), "renamed-route".to_owned());
    let recovered = test_adapter(client, root.clone());
    let WorkloadGroupObservation::Observed(observed) = recovered
        .inspect_workload_group(&renamed)
        .await
        .expect("inspect stale group after restart")
    else {
        panic!("stale group must be observed");
    };
    assert_eq!(observed.correlation, initial.correlation);

    recovered
        .remove_observed_group(&observed)
        .await
        .expect("remove stale observed group");
    assert!(matches!(
        recovered.inspect_workload_group(&renamed).await,
        Ok(WorkloadGroupObservation::Absent { .. })
    ));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_reconstructs_same_generation_reconfigurable_environment() {
    let root = test_root("restart-reconfigurable-environment");
    let client = GroupClient::default();
    let mut initial = group();
    let unit_name = WorkloadUnitName("web".to_owned());
    initial
        .units
        .get_mut(&unit_name)
        .expect("web unit")
        .environment
        .insert("ROUTE_NAME".to_owned(), "initial-route".to_owned());
    initial
        .units
        .get_mut(&unit_name)
        .expect("web unit")
        .reconfigurable_environment
        .insert("ROUTE_NAME".to_owned());
    test_adapter(client.clone(), root.clone())
        .create_workload_group(&initial)
        .await
        .expect("create initial group");

    let mut restarted_launch = initial.clone();
    restarted_launch
        .units
        .get_mut(&unit_name)
        .expect("web unit")
        .environment
        .insert("ROUTE_NAME".to_owned(), "restarted-route".to_owned());
    let restarted = test_adapter(client, root.clone());
    let recovered = restarted
        .inspect_workload_group(&restarted_launch)
        .await
        .expect("inspect same-generation group after restart");

    let WorkloadGroupObservation::Observed(observed) = recovered else {
        panic!("restarted group must be observed");
    };
    assert_eq!(observed.correlation, initial.correlation);
    restarted
        .stop_workload_group(&restarted_launch)
        .await
        .expect("stop reconstructed same-generation group");
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}
#[tokio::test]
async fn restart_reconstruction_rejects_changed_immutable_execution() {
    let root = test_root("restart-stale-execution");
    let client = GroupClient::default();
    let initial = group();
    test_adapter(client.clone(), root.clone())
        .create_workload_group(&initial)
        .await
        .expect("create initial group");
    *client.execution_image.lock().expect("execution image") =
        Some("example.test/replaced@sha256:deadbeef".to_owned());

    let mut renamed = initial;
    renamed.correlation.reconcile_generation = 2;
    assert!(
        test_adapter(client, root.clone())
            .inspect_workload_group(&renamed)
            .await
            .is_err()
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[test]
fn stale_recovery_allows_only_explicitly_reconfigurable_environment() {
    let expected = vec![
        "ROUTE_NAME=initial-route".to_owned(),
        "STATIC_VALUE=fixed".to_owned(),
    ];
    let allowed = BTreeSet::from(["ROUTE_NAME".to_owned()]);

    assert!(super::recovery::environment_matches_recovery(
        &[
            "ROUTE_NAME=renamed-route".to_owned(),
            "STATIC_VALUE=fixed".to_owned(),
        ],
        &expected,
        &allowed,
    ));
    assert!(super::recovery::environment_matches_recovery(
        &[
            "ROUTE_NAME=renamed-route".to_owned(),
            "STATIC_VALUE=fixed".to_owned(),
            "IMAGE_DEFAULT=retained".to_owned(),
        ],
        &expected,
        &allowed,
    ));
    assert!(!super::recovery::environment_matches_recovery(
        &[
            "ROUTE_NAME=renamed-route".to_owned(),
            "STATIC_VALUE=changed".to_owned(),
        ],
        &expected,
        &allowed,
    ));
    assert!(!super::recovery::environment_matches_recovery(
        &[
            "ROUTE_NAME=renamed-route".to_owned(),
            "ROUTE_NAME=duplicate".to_owned(),
            "STATIC_VALUE=fixed".to_owned(),
        ],
        &expected,
        &allowed,
    ));
}

#[tokio::test]
async fn reversible_stop_and_start_retain_group_resources_in_dependency_order() {
    let root = test_root("reversible-lifecycle");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let calls_before_stop = calls.lock().expect("calls").len();

    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop retained group");
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_stop..],
        [
            "stop:container-dotcore_site-one_group_apps-01_unit_web",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
        ]
    );
    assert!(root.join("deployment/apps-01/cache-data").is_dir());
    std::fs::remove_dir_all(root.join("deployment/apps-01/cache-data"))
        .expect("remove storage folder of the stopped group");
    let calls_before_idempotent_stop = calls.lock().expect("calls").len();
    adapter
        .stop_workload_group(&group)
        .await
        .expect("repeat stopped stop");
    assert_eq!(
        calls.lock().expect("calls").len(),
        calls_before_idempotent_stop
    );
    let calls_before_start = calls.lock().expect("calls").len();

    let WorkloadGroupObservation::Observed(started) = adapter
        .start_workload_group(&group)
        .await
        .expect("start retained group")
    else {
        panic!("retained group observation")
    };
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_start..],
        [
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
        ]
    );
    assert_eq!(started.group_id, group.group_id);
    assert_eq!(started.correlation, group.correlation);
    assert!(root.join("deployment/apps-01/cache-data").is_dir());
    assert!(
        started
            .units
            .values()
            .all(|unit| unit.state == WorkloadUnitState::Running)
    );
    let calls_before_idempotent_start = calls.lock().expect("calls").len();
    adapter
        .start_workload_group(&group)
        .await
        .expect("repeat running start");
    assert_eq!(
        calls.lock().expect("calls").len(),
        calls_before_idempotent_start
    );
    assert!(
        !calls.lock().expect("calls")[calls_before_stop..]
            .iter()
            .any(|call| call.starts_with("create:") || call.starts_with("remove:"))
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stopped_group_discovery_accepts_network_inspection_without_stopped_attachments() {
    let root = test_root("stopped-network-attachments");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop retained group");
    *client
        .omit_stopped_network_attachments
        .lock()
        .expect("network attachment behavior") = true;

    let observations = adapter
        .discover_workload_groups()
        .await
        .expect("discover stopped retained group");

    assert_eq!(observations.len(), 1);
    let WorkloadGroupObservation::Observed(observed) = &observations[0] else {
        panic!("stopped retained group was not observed: {observations:?}");
    };
    assert!(
        observed
            .units
            .values()
            .all(|unit| unit.state == WorkloadUnitState::Stopped)
    );
    assert_eq!(client.requests.lock().expect("requests").len(), 2);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn group_observation_reports_the_node_qualified_host_process_of_running_units() {
    let root = test_root("group-unit-host-process");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let web = "container-dotcore_site-one_group_apps-01_unit_web";
    client
        .process_ids
        .lock()
        .expect("process ids")
        .insert(web.to_owned(), std::process::id());

    let observations = adapter
        .discover_workload_groups()
        .await
        .expect("discover group");
    assert_eq!(observations.len(), 1);
    let WorkloadGroupObservation::Observed(observed) = &observations[0] else {
        panic!("group was not observed: {observations:?}");
    };
    // A running unit whose container process the host table can read is attributed.
    assert_eq!(
        observed.units[&WorkloadUnitName("web".to_owned())].host_process,
        expected_host_process("node-one", std::process::id())
    );
    // A running unit without a readable host PID reports none, not a guess.
    assert_eq!(
        observed.units[&WorkloadUnitName("cache".to_owned())].host_process,
        None
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn partially_stopped_group_discovery_accepts_network_inspection_without_the_stopped_unit() {
    let root = test_root("partial-stopped-network-attachments");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .running
        .lock()
        .expect("running")
        .remove("container-dotcore_site-one_group_apps-01_unit_web");
    *client
        .omit_stopped_network_attachments
        .lock()
        .expect("network attachment behavior") = true;

    let observations = adapter
        .discover_workload_groups()
        .await
        .expect("discover partially stopped group");

    assert_eq!(observations.len(), 1);
    let WorkloadGroupObservation::Observed(observed) = &observations[0] else {
        panic!("partially stopped group was not observed: {observations:?}");
    };
    assert_eq!(
        observed.units[&WorkloadUnitName("cache".to_owned())].state,
        WorkloadUnitState::Running
    );
    assert_eq!(
        observed.units[&WorkloadUnitName("web".to_owned())].state,
        WorkloadUnitState::Stopped
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn stopped_group_discovery_rejects_foreign_network_attachment_when_stopped_are_omitted() {
    let root = test_root("stopped-network-foreign-attachment");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop retained group");
    *client
        .omit_stopped_network_attachments
        .lock()
        .expect("network attachment behavior") = true;
    client
        .extra_network_attachments
        .lock()
        .expect("network attachments")
        .insert("foreign-container".to_owned());

    let error = adapter
        .discover_workload_groups()
        .await
        .expect_err("foreign network attachment must fail closed");

    assert!(error.to_string().contains("network conflicts"));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn exact_owned_partial_stops_only_active_verified_units_and_start_returns_evidence() {
    let root = test_root("reversible-partial");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .requests
        .lock()
        .expect("requests")
        .retain(|name, _| name.ends_with("_unit_cache"));
    let calls_before_stop = calls.lock().expect("calls").len();

    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop verified partial group");
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_stop..],
        ["stop:container-dotcore_site-one_group_apps-01_unit_cache"]
    );
    let calls_before_start = calls.lock().expect("calls").len();
    let WorkloadGroupObservation::Observed(observation) = adapter
        .start_workload_group(&group)
        .await
        .expect("partial group is recovery evidence")
    else {
        panic!("partial group observation")
    };
    assert_eq!(
        observation.units[&WorkloadUnitName("cache".to_owned())].state,
        WorkloadUnitState::Stopped
    );
    assert_eq!(
        observation.units[&WorkloadUnitName("web".to_owned())].state,
        WorkloadUnitState::Absent
    );
    assert_eq!(calls.lock().expect("calls").len(), calls_before_start);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn unsafe_group_evidence_causes_reversible_operations_to_fail_closed() {
    for evidence in [
        "foreign",
        "duplicate",
        "ambiguous",
        "malformed",
        "correlation",
    ] {
        let root = test_root(&format!("reversible-unsafe-{evidence}"));
        let client = GroupClient::default();
        let calls = client.calls.clone();
        let adapter = test_adapter(client.clone(), root.clone());
        let group = group();
        adapter
            .create_workload_group(&group)
            .await
            .expect("create group");
        match evidence {
            "foreign" => client
                .extra_containers
                .lock()
                .expect("extra containers")
                .push(DockerContainer {
                    id: "foreign".to_owned(),
                    name: "foreign".to_owned(),
                    name_is_unambiguous: true,
                    labels: BTreeMap::from([
                        ("com.dotcore.site".to_owned(), "site-one".to_owned()),
                        (LABEL_GROUP.to_owned(), group.group_id.0.clone()),
                    ]),
                }),
            "ambiguous" => client
                .extra_containers
                .lock()
                .expect("extra containers")
                .push(DockerContainer {
                    id: "container-dotcore_site-one_group_apps-01_unit_web".to_owned(),
                    name: "dotcore_site-one_group_apps-01_unit_web".to_owned(),
                    name_is_unambiguous: false,
                    labels: client
                        .requests
                        .lock()
                        .expect("requests")["dotcore_site-one_group_apps-01_unit_web"]
                        .labels
                        .clone(),
                }),
            "duplicate" => client
                .extra_containers
                .lock()
                .expect("extra containers")
                .push(DockerContainer {
                    id: "container-dotcore_site-one_group_apps-01_unit_web".to_owned(),
                    name: "dotcore_site-one_group_apps-01_unit_web".to_owned(),
                    name_is_unambiguous: true,
                    labels: client
                        .requests
                        .lock()
                        .expect("requests")["dotcore_site-one_group_apps-01_unit_web"]
                        .labels
                        .clone(),
                }),
            "malformed" => client
                .extra_containers
                .lock()
                .expect("extra containers")
                .push(DockerContainer {
                    id: "malformed".to_owned(),
                    name: "malformed".to_owned(),
                    name_is_unambiguous: true,
                    labels: BTreeMap::from([
                        ("com.dotcore.site".to_owned(), "site-one".to_owned()),
                        (LABEL_GROUP.to_owned(), group.group_id.0.clone()),
                        (LABEL_UNIT.to_owned(), "unknown".to_owned()),
                    ]),
                }),
            "correlation" => {
                client
                    .requests
                    .lock()
                    .expect("requests")
                    .get_mut("dotcore_site-one_group_apps-01_unit_web")
                    .expect("web request")
                    .labels
                    .insert(
                        "com.dotcore.workload.correlation".to_owned(),
                        "foreign-deployment".to_owned(),
                    );
            }
            _ => unreachable!("listed evidence"),
        }
        let calls_before = calls.lock().expect("calls").len();

        assert!(
            adapter.stop_workload_group(&group).await.is_err(),
            "{evidence}"
        );
        assert!(
            adapter.start_workload_group(&group).await.is_err(),
            "{evidence}"
        );
        assert_eq!(
            calls.lock().expect("calls").len(),
            calls_before,
            "{evidence}"
        );
        std::fs::remove_dir_all(root).expect("remove application site-data root");
    }
}

#[tokio::test]
async fn stale_group_start_returns_recovery_evidence_without_mutation() {
    let root = test_root("reversible-stale");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client, root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let mut changed = group.clone();
    changed.publication.configured_host = std::net::Ipv4Addr::new(10, 20, 30, 41);
    let calls_before = calls.lock().expect("calls").len();

    assert!(matches!(
        adapter.start_workload_group(&changed).await,
        Ok(WorkloadGroupObservation::StaleConfiguration {
            reason: WorkloadGroupStaleReason::PublicationHostChanged,
            ..
        })
    ));
    assert_eq!(calls.lock().expect("calls").len(), calls_before);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn mid_order_reversible_failures_preserve_completed_actions_for_idempotent_retry() {
    let root = test_root("reversible-retry");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.fail_stop_unit.lock().expect("stop failure") = Some("cache".to_owned());
    let calls_before_stop = calls.lock().expect("calls").len();

    let error = adapter
        .stop_workload_group(&group)
        .await
        .expect_err("cache stop failure");
    assert!(
        error
            .to_string()
            .contains("Docker group unit stop failed for cache")
    );
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_stop..],
        [
            "stop:container-dotcore_site-one_group_apps-01_unit_web",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
        ]
    );
    *client.fail_stop_unit.lock().expect("stop failure") = None;
    let calls_before_stop_retry = calls.lock().expect("calls").len();
    adapter
        .stop_workload_group(&group)
        .await
        .expect("retry only active cache");
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_stop_retry..],
        ["stop:container-dotcore_site-one_group_apps-01_unit_cache"]
    );

    *client.fail_start_unit.lock().expect("start failure") = Some("web".to_owned());
    let calls_before_start = calls.lock().expect("calls").len();
    let error = adapter
        .start_workload_group(&group)
        .await
        .expect_err("web start failure");
    assert!(
        error
            .to_string()
            .contains("Docker group unit start failed for web")
    );
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_start..],
        [
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
        ]
    );
    *client.fail_start_unit.lock().expect("start failure") = None;
    let calls_before_start_retry = calls.lock().expect("calls").len();
    adapter
        .start_workload_group(&group)
        .await
        .expect("retry complete atomic start");
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_start_retry..],
        [
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
        ]
    );

    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop before incomplete rollback");
    *client.fail_start_unit.lock().expect("start failure") = Some("web".to_owned());
    *client.fail_stop_unit.lock().expect("stop failure") = Some("cache".to_owned());
    let calls_before_incomplete_rollback = calls.lock().expect("calls").len();
    let error = adapter
        .start_workload_group(&group)
        .await
        .expect_err("start and rollback failure");
    let failure = DockerGroupStartError::from_runtime_error(&error).expect("typed start failure");
    assert!(
        failure
            .start_error()
            .to_string()
            .contains("Docker group unit start failed for web")
    );
    assert!(
        failure
            .rollback_error()
            .to_string()
            .contains("Docker group unit start rollback failed for cache")
    );
    assert_eq!(
        failure.owned_realization(),
        &OwnedWorkloadGroupRealization {
            group_id: WorkloadGroupId("apps-01".to_owned()),
            network_id: "network-dotcore_site-one_group_apps-01".to_owned(),
            units: BTreeMap::from([
                (
                    WorkloadUnitName("cache".to_owned()),
                    "container-dotcore_site-one_group_apps-01_unit_cache".to_owned(),
                ),
                (
                    WorkloadUnitName("web".to_owned()),
                    "container-dotcore_site-one_group_apps-01_unit_web".to_owned(),
                ),
            ]),
        }
    );
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_incomplete_rollback..],
        [
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
        ]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn inspection_reports_host_change_as_stale_without_docker_mutation() {
    let root = dotcore_domain::test_support::canonical_temp_dir()
        .join(format!("dotcore-group-stale-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("Application site-data root");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = DockerAdapter::new(
        client,
        DockerRuntimeConfig {
            site_id: "site-one".to_owned(),
            execution_target_id: "application-target-one".to_owned(),
            approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
            application_data_root: root.clone(),
            workload_cache_root: root.clone(),
            accelerator_transport: DockerAcceleratorTransport::Cdi,
            source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
        },
    );
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let mut changed = group.clone();
    changed.publication.configured_host = std::net::Ipv4Addr::new(10, 20, 30, 41);
    assert!(matches!(
        adapter.inspect_workload_group(&changed).await,
        Ok(WorkloadGroupObservation::StaleConfiguration {
            reason: WorkloadGroupStaleReason::PublicationHostChanged,
            ..
        })
    ));
    assert_eq!(
        calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.starts_with("create:") || call.starts_with("start:"))
            .count(),
        4
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[test]
fn empty_group_command_preserves_the_image_entrypoint_and_command() {
    let root = test_root("empty-command");
    let config = test_config(&root);
    let mut launch = group();
    let gateway = launch
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit");
    gateway.command.clear();
    gateway.arguments.clear();

    let prepared = prepare_group(&launch, &config).expect("prepare empty command");
    let request = prepared
        .units
        .get(&WorkloadUnitName("web".to_owned()))
        .expect("web request");
    assert!(request.entrypoint.is_empty());
    assert!(request.argv.is_empty());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[test]
fn prepare_group_passes_the_target_device_set_to_every_unit() {
    let root = test_root("group-devices");
    let config = test_config(&root);
    let mut launch = group();
    launch.execution_target.accelerator_device_ids =
        vec!["nvidia.com/gpu=0".to_owned(), "nvidia.com/gpu=1".to_owned()];

    let prepared = prepare_group(&launch, &config).expect("prepared group");
    let expected = vec![DockerDeviceRequest {
        driver: "cdi".to_owned(),
        device_ids: launch.execution_target.accelerator_device_ids.clone(),
        capabilities: None,
    }];
    assert_eq!(prepared.units.len(), 2);
    for unit in prepared.units.values() {
        assert_eq!(unit.device_requests, expected);
    }
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[test]
fn prepare_group_without_devices_requests_no_accelerators() {
    let root = test_root("group-devices-none");
    let prepared = prepare_group(&group(), &test_config(&root)).expect("prepared group");
    for unit in prepared.units.values() {
        assert_eq!(unit.device_requests, Vec::new());
    }
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[test]
fn group_launch_accepts_only_cdi_device_names() {
    let root = test_root("group-device-validation");
    let config = test_config(&root);
    let mut launch = group();

    launch.execution_target.accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
    validate_group_launch(&launch, &config).expect("CDI device name");

    for device in ["0", "nvidia.com/gpu"] {
        launch.execution_target.accelerator_device_ids = vec![device.to_owned()];
        validate_group_launch(&launch, &config).expect_err("not a CDI device name");
    }
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_reconstruction_rejects_a_container_with_a_different_entrypoint() {
    let root = test_root("restart-entrypoint-mismatch");
    let client = GroupClient::default();
    let initial = group();
    test_adapter(client.clone(), root.clone())
        .create_workload_group(&initial)
        .await
        .expect("create initial group");
    // A container created before the command became the entrypoint still runs the image's own.
    *client
        .execution_entrypoint
        .lock()
        .expect("execution entrypoint") = Some(vec![
        "tini".to_owned(),
        "--".to_owned(),
        "/docker-entrypoint.sh".to_owned(),
    ]);

    let mut restarted = initial;
    restarted.correlation.reconcile_generation = 2;
    let error = test_adapter(client, root.clone())
        .inspect_workload_group(&restarted)
        .await
        .expect_err("different entrypoint must not be adopted");
    assert!(
        error.to_string().contains("execution changed unexpectedly"),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(root).expect("remove Application Site-data root");
}

#[test]
fn group_command_is_the_entrypoint_and_arguments_are_the_default_arguments() {
    let root = test_root("command-and-arguments");
    let config = test_config(&root);
    let mut launch = group();
    let gateway = launch
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit");
    gateway.command = vec!["tini".to_owned(), "--".to_owned()];
    gateway.arguments = vec!["/bin/sh".to_owned(), "-c".to_owned(), "exec app".to_owned()];

    let prepared = prepare_group(&launch, &config).expect("prepare command and arguments");
    let request = prepared
        .units
        .get(&WorkloadUnitName("web".to_owned()))
        .expect("web request");
    assert_eq!(request.entrypoint, ["tini", "--"]);
    assert_eq!(request.argv, ["/bin/sh", "-c", "exec app"]);
    std::fs::remove_dir_all(root).expect("remove Application Site-data root");
}

#[test]
fn group_arguments_without_command_keep_the_image_entrypoint() {
    let root = test_root("arguments-only");
    let config = test_config(&root);
    let mut launch = group();
    let gateway = launch
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web unit");
    gateway.command.clear();
    gateway.arguments = vec!["hermes".to_owned(), "gateway".to_owned(), "run".to_owned()];

    let prepared = prepare_group(&launch, &config).expect("prepare arguments only");
    let request = prepared
        .units
        .get(&WorkloadUnitName("web".to_owned()))
        .expect("web request");
    assert!(request.entrypoint.is_empty());
    assert_eq!(request.argv, ["hermes", "gateway", "run"]);
    std::fs::remove_dir_all(root).expect("remove Application Site-data root");
}

#[tokio::test]
async fn invalid_group_input_does_not_mutate_docker_or_application_storage() {
    let root = test_root("invalid");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let mut invalid = group();
    invalid
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web")
        .environment
        .insert("invalid-name".to_owned(), "value=not-allowed".to_owned());

    assert!(adapter.create_workload_group(&invalid).await.is_err());
    assert!(calls.lock().expect("calls").is_empty());
    assert!(!root.join("deployment/apps-01").exists());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn dot_storage_names_are_rejected_before_group_resource_mutation() {
    for storage_name in [".", ".."] {
        let root = test_root(storage_name.replace('.', "dot").as_str());
        let client = GroupClient::default();
        let calls = client.calls.clone();
        let adapter = test_adapter(client, root.clone());
        let mut invalid = group();
        invalid
            .units
            .get_mut(&WorkloadUnitName("cache".to_owned()))
            .expect("cache")
            .storage[0]
            .name = storage_name.to_owned();

        assert!(adapter.create_workload_group(&invalid).await.is_err());
        assert!(calls.lock().expect("calls").is_empty());
        assert!(!root.join("deployment/apps-01").exists());
        std::fs::remove_dir_all(root).expect("remove application site-data root");
    }
}

#[test]
fn storage_symlink_component_is_rejected_before_directory_creation() {
    use std::os::unix::fs::symlink;

    let root = test_root("storage-symlink");
    let outside = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-group-storage-outside-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&outside).expect("outside directory");
    std::fs::create_dir_all(root.join("deployment")).expect("deployment namespace");
    symlink(&outside, root.join("deployment/apps-01")).expect("storage group symlink");
    let config = DockerRuntimeConfig {
        site_id: "site-one".to_owned(),
        execution_target_id: "application-target-one".to_owned(),
        approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
        application_data_root: root.clone(),
        workload_cache_root: root.clone(),
        accelerator_transport: DockerAcceleratorTransport::Cdi,
        source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
    };

    assert!(
        materialize_storage(
            &config,
            group()
                .units
                .values()
                .flat_map(|unit| &unit.storage)
                .map(Into::into),
        )
        .is_err()
    );
    assert!(!outside.join("cache-data").exists());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
    std::fs::remove_dir_all(outside).expect("remove outside directory");
}

#[test]
fn storage_file_component_is_rejected_before_directory_creation() {
    let root = test_root("storage-file");
    std::fs::create_dir_all(root.join("deployment")).expect("deployment namespace");
    std::fs::write(root.join("deployment/apps-01"), "not a directory").expect("storage group file");
    let config = DockerRuntimeConfig {
        site_id: "site-one".to_owned(),
        execution_target_id: "application-target-one".to_owned(),
        approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
        application_data_root: root.clone(),
        workload_cache_root: root.clone(),
        accelerator_transport: DockerAcceleratorTransport::Cdi,
        source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
    };

    assert!(
        materialize_storage(
            &config,
            group()
                .units
                .values()
                .flat_map(|unit| &unit.storage)
                .map(Into::into),
        )
        .is_err()
    );
    assert!(!root.join("deployment/apps-01").join("cache-data").exists());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn reconstruction_rejects_changed_immutable_execution() {
    let root = test_root("reconstruction-execution");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.execution_image.lock().expect("execution image") =
        Some("example.test/replaced@sha256:deadbeef".to_owned());
    let recovered = test_adapter(client, root.clone());

    assert!(recovered.inspect_workload_group(&group).await.is_err());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn lifecycle_action_rejects_extra_container_labels_without_mutation() {
    let root = test_root("lifecycle-extra-label");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .extra_container_labels
        .lock()
        .expect("extra labels")
        .insert("example.test/unowned".to_owned(), "unexpected".to_owned());

    assert!(adapter.remove_workload_group(&group).await.is_err());
    assert!(
        !calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.starts_with("stop:") || call.starts_with("remove:"))
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn lifecycle_action_skips_another_sites_same_named_group() {
    let root = test_root("other-site-same-group");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .extra_containers
        .lock()
        .expect("extra containers")
        .push(DockerContainer {
            id: "other-site-container".to_owned(),
            name: "other-site-container".to_owned(),
            name_is_unambiguous: false,
            labels: BTreeMap::from([
                (LABEL_GROUP.to_owned(), group.group_id.0.clone()),
                ("com.dotcore.site".to_owned(), "site-two".to_owned()),
            ]),
        });

    adapter
        .remove_workload_group(&group)
        .await
        .expect("another site resource does not block removal");
    let calls = calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .any(|call| { call == "remove:container-dotcore_site-one_group_apps-01_unit_web" })
    );
    assert!(
        calls
            .iter()
            .any(|call| { call == "remove-network:network-dotcore_site-one_group_apps-01" })
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn lifecycle_action_rejects_ambiguous_current_site_unit_without_mutation() {
    let root = test_root("ambiguous-current-site-unit");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    let request = client
        .requests
        .lock()
        .expect("requests")
        .get("dotcore_site-one_group_apps-01_unit_web")
        .expect("web request")
        .clone();
    client
        .extra_containers
        .lock()
        .expect("extra containers")
        .push(DockerContainer {
            id: "ambiguous-current-site-container".to_owned(),
            name: request.name,
            name_is_unambiguous: false,
            labels: request.labels,
        });

    assert!(adapter.remove_workload_group(&group).await.is_err());
    assert!(
        !calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.starts_with("stop:") || call.starts_with("remove:"))
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn reconstruction_rejects_extra_container_labels_without_adoption() {
    let root = test_root("reconstruction-extra-label");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .extra_container_labels
        .lock()
        .expect("extra labels")
        .insert("example.test/unowned".to_owned(), "unexpected".to_owned());
    let recovered = test_adapter(client, root.clone());

    assert!(recovered.inspect_workload_group(&group).await.is_err());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn lifecycle_action_accepts_immutable_image_labels() {
    let root = test_root("lifecycle-image-label");
    let client = GroupClient::default();
    client
        .immutable_image_labels
        .lock()
        .expect("image labels")
        .insert(
            "org.opencontainers.image.source".to_owned(),
            "https://example.test/source".to_owned(),
        );
    let adapter = test_adapter(client, root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group with image labels");

    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove group with image labels");
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn create_rejects_destination_platform_mismatch_before_realization() {
    let root = test_root("destination-platform-mismatch");
    let client = GroupClient::default();
    *client
        .destination_platform
        .lock()
        .expect("destination platform") = Some(dotcore_domain::ExecutionPlatform::linux_arm64());
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("destination mismatch");

    assert_eq!(
        error.to_string(),
        "Runtime destination platform linux/arm64 does not match retained workload platform linux/amd64"
    );
    assert!(matches!(
        error,
        RuntimeBackendError::TargetPlatformMismatch {
            required_operating_system,
            required_architecture,
            observed_operating_system,
            observed_architecture,
        } if required_operating_system == "linux"
            && required_architecture == "amd64"
            && observed_operating_system == "linux"
            && observed_architecture == "arm64"
    ));
    assert!(client.calls.lock().expect("calls").is_empty());
    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn create_rejects_exact_image_platform_mismatch_before_realization() {
    let root = test_root("image-platform-mismatch");
    let client = GroupClient::default();
    let group = group();
    let web_image = group.units[&WorkloadUnitName("web".to_owned())]
        .image
        .clone();
    client
        .image_platforms
        .lock()
        .expect("image platforms")
        .insert(
            web_image.clone(),
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        );
    let adapter = test_adapter(client.clone(), root.clone());

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("image mismatch");

    assert_eq!(
        error.to_string(),
        format!(
            "Runtime image {web_image:?} platform linux/arm64 does not match retained workload platform linux/amd64"
        )
    );
    assert!(matches!(
        error,
        RuntimeBackendError::ImagePlatformMismatch {
            image,
            required,
            observed,
        } if image == web_image
            && required == dotcore_domain::ExecutionPlatform::linux_amd64()
            && observed == dotcore_domain::ExecutionPlatform::linux_arm64()
    ));
    assert_eq!(
        client
            .platform_inspections
            .lock()
            .expect("platform inspections")
            .as_slice(),
        [
            group.units[&WorkloadUnitName("cache".to_owned())]
                .image
                .as_str(),
            group.units[&WorkloadUnitName("web".to_owned())]
                .image
                .as_str(),
        ]
    );
    assert!(client.calls.lock().expect("calls").is_empty());
    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_revalidates_destination_and_exact_images_before_start() {
    let root = test_root("restart-platform-revalidation");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop group");
    let web_image = group.units[&WorkloadUnitName("web".to_owned())]
        .image
        .clone();
    client
        .image_platforms
        .lock()
        .expect("image platforms")
        .insert(
            web_image.clone(),
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        );
    let calls_before = client.calls.lock().expect("calls").len();

    let error = adapter
        .start_workload_group(&group)
        .await
        .expect_err("foreign cached image");

    assert!(matches!(
        error,
        RuntimeBackendError::ImagePlatformMismatch { image, .. } if image == web_image
    ));
    assert_eq!(client.calls.lock().expect("calls").len(), calls_before);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn missing_container_recovery_revalidates_images_before_cleanup_or_recreate() {
    let root = test_root("recovery-platform-revalidation");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .requests
        .lock()
        .expect("requests")
        .retain(|name, _| name.ends_with("_unit_cache"));
    let recovered = test_adapter(client.clone(), root.clone());
    let web_image = group.units[&WorkloadUnitName("web".to_owned())]
        .image
        .clone();
    client
        .image_platforms
        .lock()
        .expect("image platforms")
        .insert(
            web_image.clone(),
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        );
    let calls_before = client.calls.lock().expect("calls").len();

    let error = recovered
        .create_workload_group(&group)
        .await
        .expect_err("foreign cached image blocks recovery");

    assert!(matches!(
        error,
        RuntimeBackendError::ImagePlatformMismatch { image, .. } if image == web_image
    ));
    assert_eq!(client.calls.lock().expect("calls").len(), calls_before);
    assert_eq!(client.requests.lock().expect("requests").len(), 1);
    assert!(client.network.lock().expect("network").is_some());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn successful_cleanup_removes_partial_group_evidence() {
    let root = test_root("partial");
    let client = GroupClient::default();
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    let adapter = test_adapter(client, root.clone());
    let group = group();

    assert!(adapter.create_workload_group(&group).await.is_err());
    assert!(matches!(
        adapter.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Absent { .. })
    ));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn creation_failure_cleans_up_verified_owned_resources() {
    let root = test_root("cleanup");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    let adapter = test_adapter(client, root.clone());
    let group = group();

    assert!(adapter.create_workload_group(&group).await.is_err());
    assert!(matches!(
        adapter.inspect_workload_group(&group).await,
        Ok(WorkloadGroupObservation::Absent { .. })
    ));
    let calls = calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .any(|call| call == "remove:container-dotcore_site-one_group_apps-01_unit_cache")
    );
    assert!(
        calls
            .iter()
            .any(|call| call == "remove-network:network-dotcore_site-one_group_apps-01")
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn failed_launch_retains_materialized_application_storage() {
    let root = test_root("failed-launch-retained-storage");
    let client = GroupClient::default();
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    let adapter = test_adapter(client, root.clone());

    adapter
        .create_workload_group(&group())
        .await
        .expect_err("configured launch failure");

    assert!(root.join("deployment/apps-01/cache-data").is_dir());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn failed_ready_gate_blocks_only_ready_dependents_and_returns_observation() {
    let root = test_root("ready-dependent-only");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let adapter = test_adapter(client, root.clone());
    let mut group = group();
    group
        .units
        .get_mut(&WorkloadUnitName("cache".to_owned()))
        .unwrap()
        .ready_launch_gate = Some(
        dotcore_runtime::ReadyLaunchGate::http_get(
            "http",
            "/ready",
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
        )
        .expect("gate"),
    );
    group
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .unwrap()
        .dependencies[0]
        .condition = WorkloadDependencyCondition::Ready;
    group.units.insert(
        WorkloadUnitName("worker".to_owned()),
        unit("worker", Vec::new()),
    );

    let WorkloadGroupObservation::Observed(observed) = adapter
        .create_workload_group(&group)
        .await
        .expect("failed gate is controller-visible state")
    else {
        panic!("observed group")
    };

    assert_eq!(
        observed.units[&WorkloadUnitName("cache".to_owned())].state,
        WorkloadUnitState::Running
    );
    assert_eq!(
        observed.units[&WorkloadUnitName("web".to_owned())].state,
        WorkloadUnitState::Absent
    );
    assert_eq!(
        observed.units[&WorkloadUnitName("worker".to_owned())].state,
        WorkloadUnitState::Running
    );
    assert!(
        calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.contains("unit_worker"))
    );
    assert!(root.join("deployment/apps-01/cache-data").is_dir());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn cleanup_failure_keeps_verified_partial_group_evidence_for_recovery() {
    let root = test_root("cleanup-failure");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    *client.fail_remove_unit.lock().expect("remove failure") = Some("cache".to_owned());
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("creation and cleanup failure");
    assert_eq!(
        error.to_string(),
        "Docker runtime failure: Docker group creation failed and owned-resource cleanup failed"
    );
    let failures = DockerGroupCreationError::from_runtime_error(&error)
        .expect("typed creation and cleanup failures");
    assert!(matches!(
        failures,
        DockerGroupCreationError::CreationAndCleanup { .. }
    ));
    assert!(failures.absence_inspection_error().is_none());
    assert_eq!(
        failures.owned_realization(),
        Some(&OwnedWorkloadGroupRealization {
            group_id: WorkloadGroupId("apps-01".to_owned()),
            network_id: "network-dotcore_site-one_group_apps-01".to_owned(),
            units: BTreeMap::from([(
                WorkloadUnitName("cache".to_owned()),
                "container-dotcore_site-one_group_apps-01_unit_cache".to_owned(),
            )]),
        })
    );
    let creation_error = failures.creation_error();
    assert_eq!(
        std::error::Error::source(failures)
            .expect("primary creation failure")
            .to_string(),
        creation_error.to_string()
    );
    assert_eq!(
        creation_error.to_string(),
        "Docker runtime failure: Docker group unit creation failed for web"
    );
    assert_eq!(
        creation_error
            .source()
            .expect("creation context")
            .source()
            .expect("concrete creation failure")
            .to_string(),
        "Invalid Docker Engine response: configured group create failure"
    );
    assert_eq!(
        failures.cleanup_error().to_string(),
        "Docker runtime failure: Docker partial-group removal failed"
    );
    assert_eq!(
        std::error::Error::source(failures.cleanup_error())
            .expect("cleanup context")
            .source()
            .expect("concrete cleanup failure")
            .to_string(),
        "Invalid Docker Engine response: configured group removal failure"
    );
    assert_eq!(
        *calls.lock().expect("calls"),
        [
            "network:dotcore_site-one_group_apps-01",
            "create:dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
        ]
    );
    let WorkloadGroupObservation::Observed(partial) = adapter
        .inspect_workload_group(&group)
        .await
        .expect("inspect verified partial group")
    else {
        panic!("expected partial group observation");
    };
    assert_eq!(
        partial.units[&WorkloadUnitName("cache".to_owned())].state,
        WorkloadUnitState::Stopped
    );
    assert_eq!(
        partial.units[&WorkloadUnitName("web".to_owned())].state,
        WorkloadUnitState::Absent
    );
    *client.fail_remove_unit.lock().expect("remove failure") = None;
    *client.fail_create_unit.lock().expect("create failure") = None;
    let calls_before_recovery = calls.lock().expect("calls").len();

    let WorkloadGroupObservation::Observed(recovered) = adapter
        .create_workload_group(&group)
        .await
        .expect("recover verified partial group")
    else {
        panic!("recovered group observation")
    };

    assert_eq!(recovered.group_id, group.group_id);
    assert_eq!(recovered.correlation, group.correlation);
    assert_eq!(
        recovered.backend_id,
        RuntimeBackendId("runtime/docker".to_owned())
    );
    assert_eq!(
        recovered.units,
        BTreeMap::from([
            (
                WorkloadUnitName("cache".to_owned()),
                WorkloadUnitObservation {
                    state: WorkloadUnitState::Running,
                    ready_launch_gate: None,
                    host_process: None,
                },
            ),
            (
                WorkloadUnitName("web".to_owned()),
                WorkloadUnitObservation {
                    state: WorkloadUnitState::Running,
                    ready_launch_gate: None,
                    host_process: None,
                },
            ),
        ])
    );
    assert_eq!(
        recovered.assigned_endpoint,
        Some(AssignedWorkloadEndpoint {
            unit_name: WorkloadUnitName("web".to_owned()),
            port_name: "http".to_owned(),
            configured_host: std::net::Ipv4Addr::new(10, 20, 30, 40),
            assigned_port: std::num::NonZeroU16::new(40_000).expect("non-zero port"),
        })
    );
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_recovery..],
        [
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
            "remove:container-dotcore_site-one_group_apps-01_unit_cache",
            "remove-network:network-dotcore_site-one_group_apps-01",
            "network:dotcore_site-one_group_apps-01",
            "create:dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "create:dotcore_site-one_group_apps-01_unit_web",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
        ]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn creation_cleanup_absence_inspection_failure_retains_all_failures_and_recovery_evidence() {
    let root = test_root("cleanup-absence-inspection-failure");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    *client.fail_remove_unit.lock().expect("remove failure") = Some("cache".to_owned());
    *client
        .fail_network_listing_after
        .lock()
        .expect("network listing failure") = Some(2);
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("creation, cleanup, and absence inspection failure");

    assert_eq!(
        error.to_string(),
        "Docker runtime failure: Docker group creation failed, owned-resource cleanup failed, and absence inspection failed"
    );
    let failures = DockerGroupCreationError::from_runtime_error(&error)
        .expect("typed group creation failures");
    assert!(matches!(
        failures,
        DockerGroupCreationError::CreationCleanupAndInspection { .. }
    ));
    assert_eq!(
        failures.creation_error().to_string(),
        "Docker runtime failure: Docker group unit creation failed for web"
    );
    assert_eq!(
        failures.cleanup_error().to_string(),
        "Docker runtime failure: Docker partial-group removal failed"
    );
    assert_eq!(
        failures
            .absence_inspection_error()
            .expect("retained absence inspection failure")
            .to_string(),
        "Docker runtime failure: Docker group network discovery failed"
    );
    assert_eq!(
        std::error::Error::source(failures)
            .expect("primary creation failure")
            .to_string(),
        failures.creation_error().to_string()
    );
    assert_eq!(
        failures
            .creation_error()
            .source()
            .expect("creation context")
            .source()
            .expect("concrete creation failure")
            .to_string(),
        "Invalid Docker Engine response: configured group create failure"
    );
    assert_eq!(
        failures
            .cleanup_error()
            .source()
            .expect("cleanup context")
            .source()
            .expect("concrete cleanup failure")
            .to_string(),
        "Invalid Docker Engine response: configured group removal failure"
    );
    assert_eq!(
        failures
            .absence_inspection_error()
            .expect("retained absence inspection failure")
            .source()
            .expect("inspection context")
            .source()
            .expect("concrete inspection failure")
            .to_string(),
        "Invalid Docker Engine response: configured network listing failure"
    );
    assert_eq!(
        *calls.lock().expect("calls"),
        [
            "network:dotcore_site-one_group_apps-01",
            "create:dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
        ]
    );
    assert!(
        adapter
            .known_groups()
            .lock()
            .expect("known groups")
            .contains_key(&group.group_id.0)
    );
    assert!(client.network.lock().expect("network").is_some());
    assert_eq!(client.requests.lock().expect("requests").len(), 1);
    assert!(root.join("deployment/apps-01/cache-data").is_dir());

    *client
        .fail_network_listing_after
        .lock()
        .expect("network listing failure") = None;
    *client.fail_create_unit.lock().expect("create failure") = None;
    *client.fail_remove_unit.lock().expect("remove failure") = None;
    let calls_before_recovery = calls.lock().expect("calls").len();
    let WorkloadGroupObservation::Observed(recovered) = adapter
        .create_workload_group(&group)
        .await
        .expect("recover retained partial group")
    else {
        panic!("recovered group observation");
    };
    assert_eq!(recovered.group_id, group.group_id);
    assert_eq!(recovered.correlation, group.correlation);
    assert_eq!(
        recovered.backend_id,
        RuntimeBackendId("runtime/docker".to_owned())
    );
    assert_eq!(
        recovered.units,
        BTreeMap::from([
            (
                WorkloadUnitName("cache".to_owned()),
                WorkloadUnitObservation {
                    state: WorkloadUnitState::Running,
                    ready_launch_gate: None,
                    host_process: None,
                },
            ),
            (
                WorkloadUnitName("web".to_owned()),
                WorkloadUnitObservation {
                    state: WorkloadUnitState::Running,
                    ready_launch_gate: None,
                    host_process: None,
                },
            ),
        ])
    );
    assert_eq!(
        recovered.assigned_endpoint,
        Some(AssignedWorkloadEndpoint {
            unit_name: WorkloadUnitName("web".to_owned()),
            port_name: "http".to_owned(),
            configured_host: std::net::Ipv4Addr::new(10, 20, 30, 40),
            assigned_port: std::num::NonZeroU16::new(40_000).expect("non-zero port"),
        })
    );
    assert_eq!(
        &calls.lock().expect("calls")[calls_before_recovery..],
        [
            "stop:container-dotcore_site-one_group_apps-01_unit_cache",
            "remove:container-dotcore_site-one_group_apps-01_unit_cache",
            "remove-network:network-dotcore_site-one_group_apps-01",
            "network:dotcore_site-one_group_apps-01",
            "create:dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "create:dotcore_site-one_group_apps-01_unit_web",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
        ]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn missing_image_retry_preserves_retained_partial_group_without_mutation() {
    let root = test_root("missing-image-partial-retry");
    let client = GroupClient::default();
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    *client.fail_remove_unit.lock().expect("remove failure") = Some("cache".to_owned());
    let calls = client.calls.clone();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();

    adapter
        .create_workload_group(&group)
        .await
        .expect_err("creation retains partial group when cleanup fails");
    *client.fail_create_unit.lock().expect("create failure") = None;
    *client.fail_remove_unit.lock().expect("remove failure") = None;
    client
        .missing_images
        .lock()
        .expect("missing images")
        .insert(
            group.units[&WorkloadUnitName("cache".to_owned())]
                .image
                .clone(),
        );
    let calls_before_retry = calls.lock().expect("calls").len();

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("missing image must prevent partial-group cleanup");

    assert!(matches!(error, RuntimeBackendError::ImageMissing(_)));
    assert_eq!(calls.lock().expect("calls").len(), calls_before_retry);
    assert!(client.network.lock().expect("network").is_some());
    assert_eq!(client.requests.lock().expect("requests").len(), 1);
    assert!(root.join("deployment/apps-01/cache-data").is_dir());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn creation_cleanup_retry_forgets_each_unit_deleted_before_a_later_failure() {
    let root = test_root("creation-cleanup-evidence-refresh");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let mut group = group();
    group.units.insert(
        WorkloadUnitName("worker".to_owned()),
        unit(
            "worker",
            vec![WorkloadUnitDependency {
                unit_name: WorkloadUnitName("cache".to_owned()),
                condition: WorkloadDependencyCondition::Started,
            }],
        ),
    );
    group
        .units
        .get_mut(&WorkloadUnitName("web".to_owned()))
        .expect("web")
        .dependencies = vec![WorkloadUnitDependency {
        unit_name: WorkloadUnitName("worker".to_owned()),
        condition: WorkloadDependencyCondition::Started,
    }];
    *client.fail_create_unit.lock().expect("create failure") = Some("web".to_owned());
    *client.fail_remove_unit.lock().expect("remove failure") = Some("cache".to_owned());

    adapter
        .create_workload_group(&group)
        .await
        .expect_err("worker deletion succeeds before cache deletion fails");
    assert_eq!(
        client
            .requests
            .lock()
            .expect("requests")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["dotcore_site-one_group_apps-01_unit_cache"]
    );

    *client.fail_create_unit.lock().expect("create failure") = None;
    *client.fail_remove_unit.lock().expect("remove failure") = None;
    adapter
        .create_workload_group(&group)
        .await
        .expect("in-process retry uses only retained cleanup evidence");
    assert_eq!(client.requests.lock().expect("requests").len(), 3);
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn creation_cleanup_network_delete_success_with_error_does_not_wedge_retry() {
    let root = test_root("creation-cleanup-delete-success-error");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    *client.fail_create_unit.lock().expect("create failure") = Some("cache".to_owned());
    *client
        .delete_network_then_error
        .lock()
        .expect("delete error") = true;
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("typed creation and cleanup failure");
    let failures = DockerGroupCreationError::from_runtime_error(&error)
        .expect("typed creation and cleanup failures");
    assert!(matches!(
        failures,
        DockerGroupCreationError::CreationAndCleanup { .. }
    ));
    assert!(failures.absence_inspection_error().is_none());
    assert_eq!(
        failures.cleanup_error().to_string(),
        "Docker runtime failure: Docker partial-group network removal failed"
    );
    assert_eq!(
        std::error::Error::source(failures)
            .expect("primary creation failure")
            .to_string(),
        "Docker runtime failure: Docker group unit creation failed for cache"
    );
    assert!(client.network.lock().expect("network").is_none());
    assert!(
        !adapter
            .known_groups()
            .lock()
            .expect("known groups")
            .contains_key(&group.group_id.0),
        "confirmed network absence must evict stale retry state"
    );

    *client.fail_create_unit.lock().expect("create failure") = None;
    *client
        .delete_network_then_error
        .lock()
        .expect("delete error") = false;

    let WorkloadGroupObservation::Observed(recreated) = adapter
        .create_workload_group(&group)
        .await
        .expect("retry after confirmed network absence")
    else {
        panic!("recreated group observation")
    };

    assert_eq!(recreated.group_id, group.group_id);
    assert_eq!(recreated.correlation, group.correlation);
    assert_eq!(
        recreated.backend_id,
        RuntimeBackendId("runtime/docker".to_owned())
    );
    assert_eq!(
        recreated.units,
        BTreeMap::from([
            (
                WorkloadUnitName("cache".to_owned()),
                WorkloadUnitObservation {
                    state: WorkloadUnitState::Running,
                    ready_launch_gate: None,
                    host_process: None,
                },
            ),
            (
                WorkloadUnitName("web".to_owned()),
                WorkloadUnitObservation {
                    state: WorkloadUnitState::Running,
                    ready_launch_gate: None,
                    host_process: None,
                },
            ),
        ])
    );
    assert_eq!(
        recreated.assigned_endpoint,
        Some(AssignedWorkloadEndpoint {
            unit_name: WorkloadUnitName("web".to_owned()),
            port_name: "http".to_owned(),
            configured_host: std::net::Ipv4Addr::new(10, 20, 30, 40),
            assigned_port: std::num::NonZeroU16::new(40_000).expect("non-zero port"),
        })
    );
    assert_eq!(
        *calls.lock().expect("calls"),
        [
            "network:dotcore_site-one_group_apps-01",
            "remove-network:network-dotcore_site-one_group_apps-01",
            "network:dotcore_site-one_group_apps-01",
            "create:dotcore_site-one_group_apps-01_unit_cache",
            "start:container-dotcore_site-one_group_apps-01_unit_cache",
            "create:dotcore_site-one_group_apps-01_unit_web",
            "start:container-dotcore_site-one_group_apps-01_unit_web",
        ]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_recovers_verified_partial_group_before_recreation() {
    let root = test_root("restart-partial");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    client
        .requests
        .lock()
        .expect("requests")
        .retain(|name, _| name.ends_with("_unit_cache"));
    let recovered = test_adapter(client.clone(), root.clone());

    let WorkloadGroupObservation::Observed(partial) = recovered
        .inspect_workload_group(&group)
        .await
        .expect("inspect reconstructed partial group")
    else {
        panic!("expected partial group observation");
    };
    assert_eq!(
        partial.units[&WorkloadUnitName("web".to_owned())].state,
        WorkloadUnitState::Absent
    );
    assert!(matches!(
        recovered
            .create_workload_group(&group)
            .await
            .expect("recover reconstructed partial group"),
        WorkloadGroupObservation::Observed(_)
    ));
    assert_eq!(
        client.requests.lock().expect("requests").len(),
        group.units.len()
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_reconstructs_noncanonical_detached_owned_resources_by_id() {
    let root = test_root("restart-noncanonical-detached");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    {
        let mut requests = client.requests.lock().expect("requests");
        let prior = std::mem::take(&mut *requests);
        for (name, mut request) in prior {
            request.name = format!("detached-{name}");
            request.network_name = "detached-owned-network".to_owned();
            requests.insert(request.name.clone(), request);
        }
    }
    client
        .network
        .lock()
        .expect("network")
        .as_mut()
        .expect("network")
        .name = "detached-owned-network".to_owned();
    let recovered = test_adapter(client, root.clone());

    assert!(matches!(
        recovered
            .inspect_workload_group(&group)
            .await
            .expect("reconstruct by IDs and labels"),
        WorkloadGroupObservation::Observed(_)
    ));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn uncertain_container_create_refreshes_daemon_evidence_before_cleanup() {
    let root = test_root("uncertain-create");
    let client = GroupClient::default();
    *client
        .create_unit_then_error
        .lock()
        .expect("uncertain create") = Some("cache".to_owned());
    let adapter = test_adapter(client.clone(), root.clone());

    adapter
        .create_workload_group(&group())
        .await
        .expect_err("uncertain create response");

    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn uncertain_container_remove_refreshes_daemon_evidence_and_converges() {
    let root = test_root("uncertain-remove");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client
        .remove_unit_then_error
        .lock()
        .expect("uncertain remove") = Some("web".to_owned());

    adapter
        .remove_workload_group(&group)
        .await
        .expect("confirm daemon-side removal");

    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn reconstructed_group_reports_a_valid_old_publication_host_as_stale() {
    let root = test_root("reconstructed-stale");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    let recovered = test_adapter(client, root.clone());
    let mut changed = group.clone();
    changed.publication.configured_host = std::net::Ipv4Addr::new(10, 20, 30, 41);

    assert!(matches!(
        recovered.inspect_workload_group(&changed).await,
        Ok(WorkloadGroupObservation::StaleConfiguration {
            reason: WorkloadGroupStaleReason::PublicationHostChanged,
            ..
        })
    ));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn reconstructed_group_reports_another_requested_port_as_stale() {
    let root = test_root("reconstructed-port");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    let recovered = test_adapter(client, root.clone());
    let mut changed = group.clone();
    changed.publication.host_port = NonZeroU16::new(40_001).expect("nonzero port");

    let Ok(WorkloadGroupObservation::Observed(observed)) =
        recovered.inspect_workload_group(&group).await
    else {
        panic!("unchanged publication is observed");
    };
    assert_eq!(
        observed
            .assigned_endpoint
            .map(|endpoint| endpoint.assigned_port.get()),
        Some(40_000)
    );
    assert!(matches!(
        recovered.inspect_workload_group(&changed).await,
        Ok(WorkloadGroupObservation::StaleConfiguration {
            reason: WorkloadGroupStaleReason::PublicationPortChanged,
            ..
        })
    ));
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn a_taken_published_port_fails_creation_and_start_as_port_in_use() {
    let root = test_root("port-in-use");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    *client
        .start_unit_port_in_use
        .lock()
        .expect("start port conflict") = Some("web".to_owned());

    let error = adapter
        .create_workload_group(&group)
        .await
        .expect_err("creation with a taken port");
    assert!(error.is_published_port_in_use(), "{error}");
    assert_eq!(
        error.to_string(),
        "Runtime published host port is in use on this node"
    );

    *client
        .start_unit_port_in_use
        .lock()
        .expect("start port conflict") = None;
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    adapter
        .stop_workload_group(&group)
        .await
        .expect("stop group");
    *client
        .start_unit_port_in_use
        .lock()
        .expect("start port conflict") = Some("web".to_owned());
    let error = adapter
        .start_workload_group(&group)
        .await
        .expect_err("start with a taken port");
    assert!(error.is_published_port_in_use(), "{error}");
    assert!(
        client
            .calls
            .lock()
            .expect("calls")
            .contains(&"remove:container-dotcore_site-one_group_apps-01_unit_web".to_owned())
    );
    let Ok(WorkloadGroupObservation::Observed(observed)) =
        adapter.inspect_workload_group(&group).await
    else {
        panic!("group with a discarded unit is observed");
    };
    assert_eq!(
        observed.units[&WorkloadUnitName("web".to_owned())].state,
        WorkloadUnitState::Absent
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn validated_reconstruction_retains_the_group_for_removal() {
    let root = test_root("reconstruction");
    let client = GroupClient::default();
    let calls = client.calls.clone();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    let recovered = test_adapter(client, root.clone());

    let reconstructed = recovered
        .inspect_workload_group(&group)
        .await
        .expect("inspect reconstructed group");
    assert!(matches!(
        reconstructed,
        WorkloadGroupObservation::Observed(_)
    ));
    recovered
        .remove_workload_group(&group)
        .await
        .expect("remove reconstructed group");
    let calls = calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .any(|call| call == "remove-network:network-dotcore_site-one_group_apps-01")
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_reconstruction_rejects_duplicate_exact_name_networks() {
    let root = test_root("reconstruction-duplicate-network");
    let client = GroupClient::default();
    let initial = test_adapter(client.clone(), root.clone());
    let group = group();
    initial
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.duplicate_network.lock().expect("duplicate network") = true;
    let recovered = test_adapter(client.clone(), root.clone());

    assert!(recovered.inspect_workload_group(&group).await.is_err());
    assert!(
        !client
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.starts_with("stop:") || call.starts_with("remove:"))
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn restart_recovers_an_owned_empty_network_without_recreating_it() {
    let root = test_root("restart-empty-network");
    let client = GroupClient::default();
    let group = group();
    *client.network.lock().expect("network") = Some(
        prepare_group(&group, &test_config(&root))
            .expect("prepared group")
            .network,
    );
    let recovered = test_adapter(client.clone(), root.clone());

    assert!(matches!(
        recovered
            .create_workload_group(&group)
            .await
            .expect("recover group from its retained network"),
        WorkloadGroupObservation::Observed(_)
    ));
    assert_eq!(
        client
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.starts_with("network:"))
            .count(),
        0
    );
    assert_eq!(
        client.requests.lock().expect("requests").len(),
        group.units.len()
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn cold_remove_deletes_exact_empty_network_by_id_and_repeated_absence_is_idempotent() {
    let root = test_root("cold-remove-empty-network");
    let client = GroupClient::default();
    let group = group();
    *client.network.lock().expect("network") = Some(
        prepare_group(&group, &test_config(&root))
            .expect("prepared group")
            .network,
    );
    let adapter = test_adapter(client.clone(), root.clone());

    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove residue");
    adapter
        .remove_workload_group(&group)
        .await
        .expect("repeat absence");

    assert_eq!(
        client
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.starts_with("remove-network:"))
            .cloned()
            .collect::<Vec<_>>(),
        ["remove-network:network-dotcore_site-one_group_apps-01"]
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn quiescent_recovery_rejects_mismatch_attachment_expected_name_and_duplicate_without_mutation()
 {
    for evidence in [
        "labels",
        "driver",
        "internal",
        "attachable",
        "attachment",
        "expected-name",
        "duplicate",
    ] {
        let root = test_root(evidence);
        let client = GroupClient::default();
        let group = group();
        let mut network = prepare_group(&group, &test_config(&root))
            .expect("prepared group")
            .network;
        if evidence == "labels" {
            network
                .labels
                .insert("unexpected".to_owned(), "label".to_owned());
        }
        if evidence == "driver" {
            *client.network_driver.lock().expect("driver") = Some("overlay".to_owned());
        }
        if evidence == "internal" {
            *client.network_internal.lock().expect("internal") = true;
        }
        if evidence == "attachable" {
            *client.network_attachable.lock().expect("attachable") = true;
        }
        *client.network.lock().expect("network") = Some(network);
        if evidence == "attachment" {
            client
                .extra_network_attachments
                .lock()
                .expect("attachments")
                .insert("foreign-container".to_owned());
        }
        if evidence == "expected-name" {
            client
                .existing_container_names
                .lock()
                .expect("container names")
                .insert("dotcore_site-one_group_apps-01_unit_web".to_owned());
        }
        if evidence == "duplicate" {
            *client.duplicate_network.lock().expect("duplicate network") = true;
        }
        let adapter = test_adapter(client.clone(), root.clone());

        assert!(
            adapter.create_workload_group(&group).await.is_err(),
            "{evidence}"
        );
        assert!(
            adapter.remove_workload_group(&group).await.is_err(),
            "{evidence}"
        );
        assert!(client.calls.lock().expect("calls").is_empty(), "{evidence}");
        std::fs::remove_dir_all(root).expect("remove application site-data root");
    }
}

#[tokio::test]
async fn group_validates_unit_networks_when_docker_omits_attachment_inventory() {
    let root = test_root("omitted-network-attachments");
    let client = GroupClient::default();
    *client
        .omit_network_attachments
        .lock()
        .expect("attachment inventory") = true;
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();

    assert!(matches!(
        adapter
            .create_workload_group(&group)
            .await
            .expect("create group with omitted network attachment inventory"),
        WorkloadGroupObservation::Observed(_)
    ));
    adapter
        .remove_workload_group(&group)
        .await
        .expect("remove group with omitted network attachment inventory");
    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn completed_group_requires_exact_final_network_attachments() {
    let root = test_root("final-attachments");
    let client = GroupClient::default();
    *client
        .inject_final_extra_attachment
        .lock()
        .expect("injection") = true;
    let adapter = test_adapter(client.clone(), root.clone());

    assert!(adapter.create_workload_group(&group()).await.is_err());
    assert!(
        !client
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.starts_with("remove-network:"))
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn network_deletion_conflict_does_not_claim_removal_or_delete_the_network() {
    let root = test_root("deletion-conflict");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.inject_deletion_conflict.lock().expect("conflict") = true;

    assert!(adapter.remove_workload_group(&group).await.is_err());
    assert!(client.network.lock().expect("network").is_some());
    assert!(
        !client
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.starts_with("remove-network:"))
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn known_group_removal_retry_completes_after_partial_container_success() {
    let root = test_root("partial-removal-retry");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.fail_remove_unit.lock().expect("failure") = Some("cache".to_owned());

    let error = adapter
        .remove_workload_group(&group)
        .await
        .expect_err("partial removal failure");
    assert!(error.to_string().contains("group unit removal failed"));
    assert_eq!(client.requests.lock().expect("requests").len(), 1);
    *client.fail_remove_unit.lock().expect("failure") = None;

    adapter
        .remove_workload_group(&group)
        .await
        .expect("retry partial removal");
    assert!(client.requests.lock().expect("requests").is_empty());
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn removal_retry_does_not_revalidate_units_deleted_before_a_later_failure() {
    let root = test_root("partial-removal-evidence-refresh");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.fail_remove_unit.lock().expect("failure") = Some("cache".to_owned());

    adapter
        .remove_workload_group(&group)
        .await
        .expect_err("cache removal fails after web was deleted");
    assert_eq!(client.requests.lock().expect("requests").len(), 1);

    *client.fail_remove_unit.lock().expect("failure") = None;

    adapter
        .remove_workload_group(&group)
        .await
        .expect("retry validates only remaining owned evidence");
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn network_delete_success_with_error_is_confirmed_and_retry_is_idempotent() {
    let root = test_root("network-delete-success-error");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client
        .delete_network_then_error
        .lock()
        .expect("delete error") = true;

    adapter
        .remove_workload_group(&group)
        .await
        .expect("confirm successful uncertain delete");
    adapter
        .remove_workload_group(&group)
        .await
        .expect("repeat confirmed absence");
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn network_delete_error_with_retained_exact_network_is_actionable_and_retryable() {
    let root = test_root("network-delete-retained-error");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.fail_network_remove.lock().expect("remove failure") = true;

    let error = adapter
        .remove_workload_group(&group)
        .await
        .expect_err("retained network error");
    assert!(
        error
            .to_string()
            .contains("network removal outcome is uncertain and owned network remains")
    );
    assert!(client.network.lock().expect("network").is_some());
    let observations = adapter
        .discover_workload_groups()
        .await
        .expect("discover exact network-only residue");
    assert_eq!(observations.len(), 1);
    let WorkloadGroupObservation::Observed(residue) = &observations[0] else {
        panic!("expected removable network-only residue");
    };
    assert_eq!(residue.group_id, group.group_id);
    assert!(
        residue
            .units
            .values()
            .all(|unit| unit.state == WorkloadUnitState::Absent)
    );
    assert!(residue.assigned_endpoint.is_none());
    assert!(matches!(
        adapter
            .inspect_workload_group(&group)
            .await
            .expect("inspect exact network-only residue"),
        WorkloadGroupObservation::Observed(group)
            if group.units.values().all(|unit| unit.state == WorkloadUnitState::Absent)
    ));
    *client.fail_network_remove.lock().expect("remove failure") = false;
    adapter
        .remove_workload_group(&group)
        .await
        .expect("retry retained network removal");
    assert!(client.network.lock().expect("network").is_none());
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[tokio::test]
async fn present_completion_reuses_exact_network_only_residue_after_failed_removal() {
    let root = test_root("present-network-only-completion");
    let client = GroupClient::default();
    let adapter = test_adapter(client.clone(), root.clone());
    let group = group();
    adapter
        .create_workload_group(&group)
        .await
        .expect("create group");
    *client.fail_network_remove.lock().expect("remove failure") = true;
    adapter
        .remove_workload_group(&group)
        .await
        .expect_err("retained network error");
    let retained_network = client
        .network
        .lock()
        .expect("network")
        .clone()
        .expect("retained network");
    *client.fail_network_remove.lock().expect("remove failure") = false;

    let completed = adapter
        .create_workload_group(&group)
        .await
        .expect("complete exact network-only residue");
    assert!(matches!(completed, WorkloadGroupObservation::Observed(_)));
    assert_eq!(
        client.requests.lock().expect("requests").len(),
        group.units.len()
    );
    assert_eq!(
        client
            .network
            .lock()
            .expect("network")
            .as_ref()
            .expect("reused network"),
        &retained_network
    );
    assert_eq!(
        client
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.starts_with("network:"))
            .count(),
        1
    );
    std::fs::remove_dir_all(root).expect("remove application site-data root");
}

#[test]
fn strict_endpoint_rejects_raw_malformed_and_unexpected_binding_evidence() {
    let group = group();
    let unit = KnownGroupUnit {
        container_id: "container".to_owned(),
        request: DockerGroupContainerRequest {
            name: "web".to_owned(),
            image: "image".to_owned(),
            entrypoint: Vec::new(),
            argv: Vec::new(),
            labels: BTreeMap::new(),
            ports: vec![DockerGroupPort {
                name: "http".to_owned(),
                container_port: 8080,
                publication: Some(SocketAddrV4::new(Ipv4Addr::new(10, 20, 30, 40), 40_000)),
            }],
            mounts: Vec::new(),
            device_requests: Vec::new(),
            environment: Vec::new(),
            network_name: "network".to_owned(),
            aliases: vec!["web".to_owned()],
            extra_hosts: Vec::new(),
        },
        gate: None,
    };
    for host_bindings in [
        BTreeMap::from([(
            "8080/tcp".to_owned(),
            Some(vec![super::super::client::GroupPortBinding {
                host: Some("not-an-ip".to_owned()),
                port: Some("40000".to_owned()),
            }]),
        )]),
        BTreeMap::from([
            (
                "8080/tcp".to_owned(),
                Some(vec![super::super::client::GroupPortBinding {
                    host: Some("10.20.30.40".to_owned()),
                    port: Some("40000".to_owned()),
                }]),
            ),
            (
                "9090/tcp".to_owned(),
                Some(vec![super::super::client::GroupPortBinding {
                    host: Some("10.20.30.40".to_owned()),
                    port: Some("40001".to_owned()),
                }]),
            ),
        ]),
        BTreeMap::from([(
            "8080/tcp".to_owned(),
            Some(vec![super::super::client::GroupPortBinding {
                host: Some("0.0.0.0".to_owned()),
                port: Some("40000".to_owned()),
            }]),
        )]),
    ] {
        assert!(
            strict_endpoint(
                &GroupContainerInspection {
                    running: true,
                    process_id: None,
                    host_bindings,
                    private_address: None,
                    networks: BTreeMap::new()
                },
                &unit,
                &group
            )
            .is_err()
        );
    }
}

fn alias(name: &str) -> dotcore_domain::SiteNodeHost {
    dotcore_domain::SiteNodeHost::parse(name).expect("host alias")
}

fn group() -> WorkloadGroupLaunch {
    WorkloadGroupLaunch {
        group_id: WorkloadGroupId("apps-01".to_owned()),
        correlation: WorkloadCorrelation {
            owner_id: "deployment-one".to_owned(),
            reconcile_generation: 1,
        },
        execution_target: ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-one"),
            accelerator_device_ids: Vec::new(),
        },
        execution_target_id: "application-target-one".to_owned(),
        required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
        units: BTreeMap::from([
            (
                WorkloadUnitName("cache".to_owned()),
                unit("cache", Vec::new()),
            ),
            (
                WorkloadUnitName("web".to_owned()),
                unit(
                    "web",
                    vec![WorkloadUnitDependency {
                        unit_name: WorkloadUnitName("cache".to_owned()),
                        condition: WorkloadDependencyCondition::Started,
                    }],
                ),
            ),
        ]),
        publication: WorkloadPublicationRequest {
            unit_name: WorkloadUnitName("web".to_owned()),
            port_name: "http".to_owned(),
            configured_host: std::net::Ipv4Addr::new(10, 20, 30, 40),
            host_port: NonZeroU16::new(40_000).expect("nonzero port"),
        },
        host_aliases: Default::default(),
    }
}

fn group_for_target(
    group_id: &str,
    owner_id: &str,
    execution_target_id: &str,
) -> WorkloadGroupLaunch {
    let mut group = group();
    group.group_id = WorkloadGroupId(group_id.to_owned());
    for storage in group.units.values_mut().flat_map(|unit| &mut unit.storage) {
        storage.key = group_id.to_owned();
    }
    group.correlation.owner_id = owner_id.to_owned();
    group.execution_target_id = execution_target_id.to_owned();
    group
}

fn model_container(execution_target_id: &str) -> DockerContainer {
    let workload_id = "dotcore_site-one_model-one_1";
    DockerContainer {
        id: "model-container".to_owned(),
        name: workload_id.to_owned(),
        name_is_unambiguous: true,
        labels: BTreeMap::from([
            (LABEL_SITE.to_owned(), "site-one".to_owned()),
            (
                LABEL_EXECUTION_TARGET.to_owned(),
                execution_target_id.to_owned(),
            ),
            (LABEL_MANAGED.to_owned(), "true".to_owned()),
            (LABEL_CORRELATION.to_owned(), "model-one".to_owned()),
            (LABEL_GENERATION.to_owned(), "1".to_owned()),
            (LABEL_SCHEMA.to_owned(), OWNERSHIP_SCHEMA.to_owned()),
            (LABEL_WORKLOAD.to_owned(), workload_id.to_owned()),
            (
                LABEL_LAUNCH_FINGERPRINT.to_owned(),
                "fingerprint".to_owned(),
            ),
            (LABEL_PORTS.to_owned(), String::new()),
        ]),
    }
}

fn reverse_lexical_group() -> WorkloadGroupLaunch {
    WorkloadGroupLaunch {
        group_id: WorkloadGroupId("apps-01".to_owned()),
        correlation: WorkloadCorrelation {
            owner_id: "deployment-one".to_owned(),
            reconcile_generation: 1,
        },
        execution_target: ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-one"),
            accelerator_device_ids: Vec::new(),
        },
        execution_target_id: "application-target-one".to_owned(),
        required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
        units: BTreeMap::from([
            (
                WorkloadUnitName("alpha".to_owned()),
                unit(
                    "alpha",
                    vec![WorkloadUnitDependency {
                        unit_name: WorkloadUnitName("zulu".to_owned()),
                        condition: WorkloadDependencyCondition::Started,
                    }],
                ),
            ),
            (
                WorkloadUnitName("zulu".to_owned()),
                unit("zulu", Vec::new()),
            ),
        ]),
        publication: WorkloadPublicationRequest {
            unit_name: WorkloadUnitName("alpha".to_owned()),
            port_name: "http".to_owned(),
            configured_host: std::net::Ipv4Addr::new(10, 20, 30, 40),
            host_port: NonZeroU16::new(40_000).expect("nonzero port"),
        },
        host_aliases: Default::default(),
    }
}

fn test_root(name: &str) -> PathBuf {
    let root = dotcore_domain::test_support::canonical_temp_dir()
        .join(format!("dotcore-group-{name}-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("Application site-data root");
    root
}

fn test_adapter(client: GroupClient, root: PathBuf) -> DockerAdapter<GroupClient> {
    adapter_for_target(client, root, "application-target-one")
}

fn adapter_for_target(
    client: GroupClient,
    root: PathBuf,
    execution_target_id: &str,
) -> DockerAdapter<GroupClient> {
    let mut config = test_config(&root);
    config.execution_target_id = execution_target_id.to_owned();
    DockerAdapter::new(client, config)
}

fn test_config(root: &std::path::Path) -> DockerRuntimeConfig {
    DockerRuntimeConfig {
        site_id: "site-one".to_owned(),
        execution_target_id: "application-target-one".to_owned(),
        approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
        application_data_root: root.to_owned(),
        workload_cache_root: root.to_owned(),
        accelerator_transport: DockerAcceleratorTransport::Cdi,
        source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
    }
}

fn unit(name: &str, dependencies: Vec<WorkloadUnitDependency>) -> WorkloadUnitLaunch {
    WorkloadUnitLaunch {
        image: format!("example.test/{name}@sha256:{}", "a".repeat(64)),
        command: vec!["/bin/server".to_owned()],
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        reconfigurable_environment: BTreeSet::new(),
        storage: (name == "cache")
            .then_some(WorkloadStorageAttachment {
                name: "cache-data".to_owned(),
                destination: "/var/lib/cache".to_owned(),
                kind: WorkloadStorageKind::Data,
                namespace: "deployment".to_owned(),
                key: "apps-01".to_owned(),
                read_only: false,
                expires_when_unused: false,
            })
            .into_iter()
            .collect(),
        ports: vec![WorkloadPort {
            name: "http".to_owned(),
            container_port: 8080,
        }],
        dependencies,
        ready_launch_gate: None,
    }
}

type GroupLogFixtures = BTreeMap<String, Vec<Result<DockerLogOutput, ClientError>>>;

#[derive(Clone, Default)]
struct GroupClient {
    calls: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<BTreeMap<String, DockerGroupContainerRequest>>>,
    network_created: Arc<Mutex<bool>>,
    network: Arc<Mutex<Option<DockerGroupNetworkRequest>>>,
    networks: Arc<Mutex<BTreeMap<String, DockerGroupNetworkRequest>>>,
    fail_create_unit: Arc<Mutex<Option<String>>>,
    create_unit_not_found: Arc<Mutex<Option<String>>>,
    fail_start_unit: Arc<Mutex<Option<String>>>,
    start_unit_port_in_use: Arc<Mutex<Option<String>>>,
    destination_platform: Arc<Mutex<Option<dotcore_domain::ExecutionPlatform>>>,
    image_platforms: Arc<Mutex<BTreeMap<String, dotcore_domain::ExecutionPlatform>>>,

    fail_stop_unit: Arc<Mutex<Option<String>>>,
    create_unit_then_error: Arc<Mutex<Option<String>>>,
    fail_remove_unit: Arc<Mutex<Option<String>>>,
    remove_unit_then_error: Arc<Mutex<Option<String>>>,
    execution_image: Arc<Mutex<Option<String>>>,
    execution_entrypoint: Arc<Mutex<Option<Vec<String>>>>,
    immutable_image_labels: Arc<Mutex<BTreeMap<String, String>>>,
    extra_container_labels: Arc<Mutex<BTreeMap<String, String>>>,
    extra_containers: Arc<Mutex<Vec<DockerContainer>>>,
    existing_container_names: Arc<Mutex<BTreeSet<String>>>,
    missing_images: Arc<Mutex<BTreeSet<String>>>,
    image_inspections: Arc<Mutex<Vec<String>>>,
    platform_inspections: Arc<Mutex<Vec<String>>>,
    extra_network_attachments: Arc<Mutex<BTreeSet<String>>>,
    omit_network_attachments: Arc<Mutex<bool>>,
    duplicate_network: Arc<Mutex<bool>>,
    inject_final_extra_attachment: Arc<Mutex<bool>>,
    omit_stopped_network_attachments: Arc<Mutex<bool>>,
    inject_deletion_conflict: Arc<Mutex<bool>>,
    network_driver: Arc<Mutex<Option<String>>>,
    network_internal: Arc<Mutex<bool>>,
    network_attachable: Arc<Mutex<bool>>,
    delete_network_then_error: Arc<Mutex<bool>>,
    fail_network_remove: Arc<Mutex<bool>>,
    fail_network_listing_after: Arc<Mutex<Option<usize>>>,
    running: Arc<Mutex<BTreeSet<String>>>,
    process_ids: Arc<Mutex<BTreeMap<String, u32>>>,
    logs: Arc<Mutex<GroupLogFixtures>>,
    log_requests: Arc<Mutex<Vec<DockerLogRequest>>>,
    pending_logs: Arc<Mutex<BTreeSet<String>>>,
    pending_ownership: Arc<Mutex<BTreeSet<String>>>,
}

impl GroupClient {
    fn network_requests(&self) -> (Vec<DockerGroupNetworkRequest>, bool) {
        let networks = self.networks.lock().expect("networks");
        if networks.len() > 1 {
            return (networks.values().cloned().collect(), true);
        }
        (
            self.network
                .lock()
                .expect("network")
                .clone()
                .into_iter()
                .collect(),
            false,
        )
    }
}

impl DockerClient for GroupClient {
    fn list_managed_containers(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send {
        let mut containers = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .map(|request| DockerContainer {
                id: format!("container-{}", request.name),
                name: request.name.clone(),
                name_is_unambiguous: true,
                labels: container_labels(
                    request.labels.clone(),
                    &self.immutable_image_labels,
                    &self.extra_container_labels,
                ),
            })
            .collect::<Vec<_>>();
        containers.extend(
            self.extra_containers
                .lock()
                .expect("extra containers")
                .clone(),
        );
        ready(Ok(containers))
    }
    fn create_container(
        &self,
        _: &DockerCreateRequest,
    ) -> impl std::future::Future<Output = Result<String, ClientError>> + Send {
        ready(Err(ClientError::new("unused")))
    }
    fn container_logs(&self, request: DockerLogRequest) -> DockerLogStream {
        self.log_requests
            .lock()
            .expect("log requests")
            .push(request.clone());
        if self
            .pending_logs
            .lock()
            .expect("pending logs")
            .contains(&request.container_id)
        {
            return Box::pin(futures_util::stream::pending());
        }
        Box::pin(futures_util::stream::iter(
            self.logs
                .lock()
                .expect("logs")
                .remove(&request.container_id)
                .unwrap_or_default(),
        ))
    }
    fn inspect_owned_container(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<DockerContainer, ClientError>> + Send {
        let request = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .find(|request| format!("container-{}", request.name) == id || request.name == id)
            .cloned();
        ready(
            request
                .map(|request| DockerContainer {
                    id: format!("container-{}", request.name),
                    name: request.name,
                    name_is_unambiguous: true,
                    labels: container_labels(
                        request.labels,
                        &self.immutable_image_labels,
                        &self.extra_container_labels,
                    ),
                })
                .ok_or_else(|| ClientError::not_found("missing")),
        )
    }
    fn container_exists(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<bool, ClientError>> + Send {
        ready(Ok(self
            .existing_container_names
            .lock()
            .expect("container names")
            .contains(name)))
    }
    fn image_exists(
        &self,
        image: &str,
    ) -> impl std::future::Future<Output = Result<bool, ClientError>> + Send {
        self.image_inspections
            .lock()
            .expect("image inspections")
            .push(image.to_owned());
        ready(Ok(!self
            .missing_images
            .lock()
            .expect("missing images")
            .contains(image)))
    }
    fn execution_platform(
        &self,
    ) -> impl std::future::Future<Output = Result<dotcore_domain::ExecutionPlatform, ClientError>> + Send
    {
        ready(Ok(self
            .destination_platform
            .lock()
            .expect("destination platform")
            .clone()
            .unwrap_or_else(
                dotcore_domain::ExecutionPlatform::linux_amd64,
            )))
    }
    fn inspect_image_platform(
        &self,
        image: &str,
    ) -> impl std::future::Future<Output = Result<dotcore_domain::ExecutionPlatform, ClientError>> + Send
    {
        self.platform_inspections
            .lock()
            .expect("platform inspections")
            .push(image.to_owned());
        ready(Ok(self
            .image_platforms
            .lock()
            .expect("image platforms")
            .get(image)
            .cloned()
            .unwrap_or_else(
                dotcore_domain::ExecutionPlatform::linux_amd64,
            )))
    }
    fn start_container(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<(), ClientError>> + Send {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("start:{id}"));
        if self
            .fail_start_unit
            .lock()
            .expect("start failure")
            .as_deref()
            .is_some_and(|unit| id.ends_with(unit))
        {
            return ready(Err(ClientError::new("configured group start failure")));
        }
        if self
            .start_unit_port_in_use
            .lock()
            .expect("start port conflict")
            .as_deref()
            .is_some_and(|unit| id.ends_with(unit))
        {
            return ready(Err(ClientError::Docker(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 500,
                    message: "driver failed programming external connectivity on endpoint web: Bind for 10.20.30.40:40000 failed: port is already allocated".to_owned(),
                },
            )));
        }
        self.running.lock().expect("running").insert(id.to_owned());
        ready(Ok(()))
    }
    fn inspect_container(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Result<ContainerInspection, ClientError>> + Send {
        ready(Err(ClientError::new("unused")))
    }
    fn stop_container(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<(), ClientError>> + Send {
        self.calls.lock().expect("calls").push(format!("stop:{id}"));
        if self
            .fail_stop_unit
            .lock()
            .expect("stop failure")
            .as_deref()
            .is_some_and(|unit| id.ends_with(unit))
        {
            return ready(Err(ClientError::new("configured group stop failure")));
        }
        self.running.lock().expect("running").remove(id);
        ready(Ok(()))
    }
    fn remove_container(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<(), ClientError>> + Send {
        if self
            .fail_remove_unit
            .lock()
            .expect("remove failure")
            .as_deref()
            .is_some_and(|unit| id.ends_with(unit))
        {
            return ready(Err(ClientError::new("configured group removal failure")));
        }
        self.calls
            .lock()
            .expect("calls")
            .push(format!("remove:{id}"));
        self.requests
            .lock()
            .expect("requests")
            .retain(|_, request| format!("container-{}", request.name) != id);
        self.running.lock().expect("running").remove(id);
        if self
            .remove_unit_then_error
            .lock()
            .expect("uncertain remove")
            .as_deref()
            .is_some_and(|unit| id.ends_with(unit))
        {
            return ready(Err(ClientError::new(
                "configured successful removal response failure",
            )));
        }
        if *self.inject_deletion_conflict.lock().expect("conflict")
            && self.requests.lock().expect("requests").is_empty()
        {
            self.extra_network_attachments
                .lock()
                .expect("attachments")
                .insert("conflicting-container".to_owned());
        }
        ready(Ok(()))
    }
    fn create_group_network(
        &self,
        request: &DockerGroupNetworkRequest,
    ) -> impl std::future::Future<Output = Result<String, ClientError>> + Send {
        *self.network_created.lock().expect("network") = true;
        *self.network.lock().expect("network") = Some(request.clone());
        self.networks
            .lock()
            .expect("networks")
            .insert(request.name.clone(), request.clone());
        self.calls
            .lock()
            .expect("calls")
            .push(format!("network:{}", request.name));
        ready(Ok(format!("network-{}", request.name)))
    }
    fn inspect_owned_network(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<DockerNetwork, ClientError>> + Send {
        let (networks, isolate_attachments) = self.network_requests();
        let network = networks
            .into_iter()
            .find(|network| format!("network-{}", network.name) == id || network.name == id);
        let running = self.running.lock().expect("running").clone();
        let omit_stopped = *self
            .omit_stopped_network_attachments
            .lock()
            .expect("network attachment behavior");
        let mut attached_container_ids = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .filter(|request| {
                (!isolate_attachments
                    || network
                        .as_ref()
                        .is_some_and(|network| request.network_name == network.name))
                    && (!omit_stopped || running.contains(&format!("container-{}", request.name)))
            })
            .map(|request| format!("container-{}", request.name))
            .collect::<BTreeSet<_>>();
        attached_container_ids.extend(
            self.extra_network_attachments
                .lock()
                .expect("attachments")
                .iter()
                .cloned(),
        );
        if *self
            .omit_network_attachments
            .lock()
            .expect("attachment inventory")
        {
            attached_container_ids.clear();
        }
        ready(
            network
                .map(|network| DockerNetwork {
                    id: format!("network-{}", network.name),
                    name: network.name,
                    labels: network.labels,
                    internal: *self.network_internal.lock().expect("internal"),
                    attachable: *self.network_attachable.lock().expect("attachable"),
                    driver: self
                        .network_driver
                        .lock()
                        .expect("driver")
                        .clone()
                        .unwrap_or_else(|| "bridge".to_owned()),
                    attached_container_ids,
                })
                .filter(|network| network.id == id || network.name == id)
                .ok_or_else(|| ClientError::not_found("missing")),
        )
    }
    fn list_group_networks(
        &self,
        network_name: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DockerNetwork>, ClientError>> + Send {
        let mut remaining_successes = self
            .fail_network_listing_after
            .lock()
            .expect("network listing failure");
        if let Some(remaining) = remaining_successes.as_mut() {
            if *remaining == 0 {
                return ready(Err(ClientError::new("configured network listing failure")));
            }
            *remaining -= 1;
        }
        let (networks, isolate_attachments) = self.network_requests();
        let network = networks
            .into_iter()
            .find(|network| network.name == network_name);
        let running = self.running.lock().expect("running").clone();
        let omit_stopped = *self
            .omit_stopped_network_attachments
            .lock()
            .expect("network attachment behavior");
        let mut attached_container_ids = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .filter(|request| {
                (!isolate_attachments
                    || network
                        .as_ref()
                        .is_some_and(|network| request.network_name == network.name))
                    && (!omit_stopped || running.contains(&format!("container-{}", request.name)))
            })
            .map(|request| format!("container-{}", request.name))
            .collect::<BTreeSet<_>>();
        attached_container_ids.extend(
            self.extra_network_attachments
                .lock()
                .expect("attachments")
                .iter()
                .cloned(),
        );
        if *self
            .omit_network_attachments
            .lock()
            .expect("attachment inventory")
        {
            attached_container_ids.clear();
        }
        let mut networks = network
            .filter(|network| network.name == network_name)
            .map(|network| DockerNetwork {
                id: format!("network-{}", network.name),
                name: network.name,
                labels: network.labels,
                internal: *self.network_internal.lock().expect("internal"),
                attachable: *self.network_attachable.lock().expect("attachable"),
                driver: self
                    .network_driver
                    .lock()
                    .expect("driver")
                    .clone()
                    .unwrap_or_else(|| "bridge".to_owned()),
                attached_container_ids,
            })
            .into_iter()
            .collect::<Vec<_>>();
        if *self.duplicate_network.lock().expect("duplicate network")
            && let Some(network) = networks.first().cloned()
        {
            networks.push(network);
        }
        ready(Ok(networks))
    }
    fn list_managed_group_network_summaries(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<DockerNetworkSummary>, ClientError>> + Send
    {
        let summaries = self
            .network
            .lock()
            .expect("network")
            .clone()
            .map(|network| DockerNetworkSummary {
                id: format!("network-{}", network.name),
                name: network.name,
                labels: network.labels,
            })
            .into_iter()
            .collect();
        ready(Ok(summaries))
    }

    fn list_managed_group_networks(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<DockerNetwork>, ClientError>> + Send {
        let names = self
            .network_requests()
            .0
            .into_iter()
            .map(|network| network.name)
            .collect::<Vec<_>>();
        let this = self.clone();
        async move {
            let mut networks = Vec::new();
            for name in names {
                networks.extend(this.list_group_networks(&name).await?);
            }
            Ok(networks)
        }
    }
    fn remove_network(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<(), ClientError>> + Send {
        if !self
            .extra_network_attachments
            .lock()
            .expect("attachments")
            .is_empty()
        {
            return ready(Err(ClientError::new("network has active endpoints")));
        }
        if *self.fail_network_remove.lock().expect("remove failure") {
            return ready(Err(ClientError::new("configured network removal failure")));
        }
        self.calls
            .lock()
            .expect("calls")
            .push(format!("remove-network:{id}"));
        let mut networks = self.networks.lock().expect("networks");
        networks.retain(|name, _| format!("network-{name}") != id && name != id);
        let mut network = self.network.lock().expect("network");
        if network
            .as_ref()
            .is_some_and(|network| format!("network-{}", network.name) == id || network.name == id)
        {
            *network = networks.values().next().cloned();
        }
        *self.network_created.lock().expect("network") = network.is_some();
        if *self.delete_network_then_error.lock().expect("delete error") {
            return ready(Err(ClientError::new(
                "configured successful delete response failure",
            )));
        }
        ready(Ok(()))
    }
    fn create_group_container(
        &self,
        request: &DockerGroupContainerRequest,
    ) -> impl std::future::Future<Output = Result<String, ClientError>> + Send {
        if self
            .fail_create_unit
            .lock()
            .expect("create failure")
            .as_deref()
            .is_some_and(|unit| request.name.ends_with(unit))
        {
            return ready(Err(ClientError::new("configured group create failure")));
        }
        if self
            .create_unit_not_found
            .lock()
            .expect("create not-found")
            .as_deref()
            .is_some_and(|unit| request.name.ends_with(unit))
        {
            return ready(Err(ClientError::not_found(format!(
                "No such image: {}",
                request.image
            ))));
        }
        self.calls
            .lock()
            .expect("calls")
            .push(format!("create:{}", request.name));
        self.requests
            .lock()
            .expect("requests")
            .insert(request.name.clone(), request.clone());
        if self
            .create_unit_then_error
            .lock()
            .expect("uncertain create")
            .as_deref()
            .is_some_and(|unit| request.name.ends_with(unit))
        {
            return ready(Err(ClientError::new(
                "configured successful create response failure",
            )));
        }
        if *self
            .inject_final_extra_attachment
            .lock()
            .expect("injection")
            && request.name.ends_with("_unit_web")
        {
            self.extra_network_attachments
                .lock()
                .expect("attachments")
                .insert("unexpected-container".to_owned());
        }
        ready(Ok(format!("container-{}", request.name)))
    }
    fn inspect_group_container(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<GroupContainerInspection, ClientError>> + Send
    {
        let request = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .find(|request| format!("container-{}", request.name) == id)
            .cloned()
            .expect("group request");
        let host_bindings = request
            .ports
            .into_iter()
            .filter_map(|port| {
                port.publication.map(|address| {
                    (
                        format!("{}/tcp", port.container_port),
                        Some(vec![super::super::client::GroupPortBinding {
                            host: Some(address.ip().to_string()),
                            port: Some(address.port().to_string()),
                        }]),
                    )
                })
            })
            .collect();
        let running = self.running.lock().expect("running").contains(id);
        let process_id = self
            .process_ids
            .lock()
            .expect("process ids")
            .get(id)
            .copied();
        ready(Ok(GroupContainerInspection {
            running,
            process_id,
            host_bindings: if running {
                host_bindings
            } else {
                BTreeMap::new()
            },
            private_address: None,
            networks: BTreeMap::from([(
                request.network_name.clone(),
                GroupNetworkAttachment {
                    network_id: Some(format!("network-{}", request.network_name)),
                    aliases: Some(request.aliases),
                },
            )]),
        }))
    }
    fn inspect_group_container_ownership(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<GroupContainerOwnership, ClientError>> + Send
    {
        let pending = self
            .pending_ownership
            .lock()
            .expect("pending ownership")
            .contains(id);
        let request = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .find(|request| format!("container-{}", request.name) == id)
            .cloned();
        let image_labels = self
            .immutable_image_labels
            .lock()
            .expect("image labels")
            .clone();
        let extra_labels = self
            .extra_container_labels
            .lock()
            .expect("extra labels")
            .clone();
        async move {
            if pending {
                std::future::pending().await
            } else {
                request
                    .map(|request| GroupContainerOwnership {
                        id: format!("container-{}", request.name),
                        name: request.name,
                        labels: container_labels_from_maps(
                            request.labels,
                            &image_labels,
                            &extra_labels,
                        ),
                        image_labels,
                    })
                    .ok_or_else(|| ClientError::not_found("missing"))
            }
        }
    }
    fn inspect_group_execution(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<DockerGroupExecution, ClientError>> + Send {
        let request = self
            .requests
            .lock()
            .expect("requests")
            .values()
            .find(|request| format!("container-{}", request.name) == id)
            .cloned();
        let image = self
            .execution_image
            .lock()
            .expect("execution image")
            .clone();
        let entrypoint = self
            .execution_entrypoint
            .lock()
            .expect("execution entrypoint")
            .clone();
        ready(
            request
                .map(|request| DockerGroupExecution {
                    image: image.unwrap_or(request.image),
                    entrypoint: entrypoint.unwrap_or(request.entrypoint),
                    argv: request.argv,
                    environment: request.environment,
                    mounts: request.mounts,
                })
                .ok_or_else(|| ClientError::not_found("missing")),
        )
    }
}

fn container_labels(
    mut labels: BTreeMap<String, String>,
    image_labels: &Arc<Mutex<BTreeMap<String, String>>>,
    extra: &Arc<Mutex<BTreeMap<String, String>>>,
) -> BTreeMap<String, String> {
    labels = container_labels_from_maps(
        labels,
        &image_labels.lock().expect("image labels"),
        &extra.lock().expect("extra labels"),
    );
    labels
}

fn container_labels_from_maps(
    ownership_labels: BTreeMap<String, String>,
    image_labels: &BTreeMap<String, String>,
    extra_labels: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut labels = image_labels.clone();
    labels.extend(ownership_labels);
    labels.extend(extra_labels.clone());
    labels
}

/// Real-Docker test site whose labeled containers, networks, and data root are removed on drop,
/// so a failing test leaves nothing behind.
struct DockerSiteCleanup {
    site_id: String,
    root: PathBuf,
}

impl Drop for DockerSiteCleanup {
    fn drop(&mut self) {
        let label = format!("label=com.dotcore.site={}", self.site_id);
        let containers: (&[&str], &[&str]) = (&["ps", "-aq", "--filter"], &["rm", "-f"]);
        let networks: (&[&str], &[&str]) =
            (&["network", "ls", "-q", "--filter"], &["network", "rm"]);
        // Containers go first, since a network is removable only once nothing is attached.
        for (list, remove) in [containers, networks] {
            let listed = docker(&[list, &[label.as_str()]].concat()).stdout;
            for id in String::from_utf8_lossy(&listed).split_whitespace() {
                let _ = docker(&[remove, &[id]].concat());
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn docker(arguments: &[&str]) -> std::process::Output {
    std::process::Command::new("docker")
        .args(arguments)
        .output()
        .expect("docker command")
}

/// The host identity a node resolves for one process; only Linux reads container PIDs from the
/// host process table, where macOS containers run inside Docker's Linux virtual machine.
#[cfg(target_os = "linux")]
fn expected_host_process(
    node_id: &str,
    process_id: u32,
) -> Option<dotcore_domain::host_process::HostProcessIdentity> {
    Some(dotcore_domain::host_process::HostProcessIdentity {
        managed_node_id: SiteNodeId::for_test(node_id),
        process_id,
        process_start_token: dotcore_host_process::read_process_start_token(process_id)
            .expect("process start token"),
    })
}

#[cfg(not(target_os = "linux"))]
fn expected_host_process(
    _node_id: &str,
    _process_id: u32,
) -> Option<dotcore_domain::host_process::HostProcessIdentity> {
    None
}
