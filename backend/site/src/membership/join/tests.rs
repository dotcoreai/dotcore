//! End-to-end node join tests: the join registry, served on the manager's private connections,
//! and the joining node talk over loopback TLS with real site certificates, recording into an
//! initialized site through its repository.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, mpsc},
    time::Duration,
};

use dotcore_domain::{
    ExecutionPlatform, HostResolver, SiteNodeHost,
    clock::{ManualClock, SystemClock},
    test_support::{
        host_resolver::FakeHostResolver,
        ports::{ReservedPort, reserve_port},
    },
    timing::{JoinTiming, Timing},
};
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::{
    node::v1::{
        CommitRequest, DeclaredMachine, JoinRequest, JoinResponse, NodeObservationConfiguration,
        node_join_service_client::NodeJoinServiceClient,
    },
    observation::v1::{AcceleratorFacts, AcceleratorVendor},
    site::v1::{
        ExecutionTarget, HardwareObservationConfiguration, NativeWorkloadLifetime, Site, SiteNode,
        execution_target,
    },
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use tonic::{
    Code,
    transport::{Channel, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig},
};

use super::{
    FingerprintColor, FingerprintPattern, JOIN_SECRET_BYTES, JoinAttempt, JoinAttemptOutcome,
    JoinConnection, JoinHostIdentity, JoinHostSource, JoinLimits, JoinProposal, JoinRegistry,
    JoinRegistryError, JoinReview, JoinSecret, JoinSiteError, JoinSiteWriter, JoinStatus,
    JoinToken, JoinedNode, NodeFolderError, NodeJoinError, NodeKey, NodeRejoin, RejoinProposal,
    RejoiningNodeFolder, ReviewedJoin,
    dial_back_listener::DialBackListener,
    host::JoinDecision,
    test_fixtures::{InitializedSite, initialized_site, initialized_site_with_manager_host},
    verifier::JoinHostVerifier,
};
use crate::{
    SITE_FILE,
    atomic::{FileSystem, StdFileSystem},
    initial::{
        inputs::{ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices},
        request::{InitialApplicationRuntime, derive_site_node_id},
    },
    manager_trust::{
        ManagerTrust, SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY, validate_for_private_startup,
    },
    membership::MembershipRecorder,
    node::folder::{
        CertificateFingerprint, NODE_CERTIFICATE_FILE, NODE_PRIVATE_KEY_FILE, NodeFolderValidator,
        WORKER_MANIFEST_FILE, WorkerNodeCustody,
    },
    private_transport::{
        JoinGate, PeerPins, PinnedPeerVerifier, PrivateConnectionInfo, PrivateConnections,
        PrivateNodeRole, PrivatePeerPolicy, PrivateServerConfiguration, UnpinnedConnectionLimits,
        pem,
    },
    repository::{SiteRepository, SiteWriteFault, SiteWriteFaults},
    validation::load_validated_site,
};

#[tokio::test]
async fn successful_join_installs_a_startable_folder_and_records_exactly_one_node() {
    let harness = Harness::start(Options::default()).await;
    let changed = harness.repository.register_change_listener();
    let key = NodeKey::generate().expect("key");
    let node_root = harness.node_root();
    let mut connection = harness.connect().await;

    assert_eq!(connection.site_id(), harness.fixture.site.site_id);
    assert_eq!(connection.site_ca_sha256(), harness.token.site_ca_sha256);
    let joined = connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect("join");
    let recorded = harness.finish().await.expect("host result");

    let expected_node_id = derive_site_node_id(&harness.fixture.site.site_id, "worker");
    assert_eq!(joined.node_id, expected_node_id);
    assert_eq!(joined.site_id, harness.fixture.site.site_id);
    assert_eq!(joined.root, node_root);
    assert_eq!(joined.host, host(WORKER_HOST));
    assert_eq!(
        joined.private_bind_address.to_string(),
        format!("{WORKER_HOST}:{}", worker_port())
    );

    let folder = NodeFolderValidator::validate_declared(&node_root).expect("valid node folder");
    assert_eq!(folder.worker_node_id(), expected_node_id);
    assert_eq!(folder.private_bind_address(), &joined.private_bind_address);
    assert_eq!(
        folder.declared_machine(),
        Some(
            MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
                .expect("simulated machine")
        )
    );
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://127.0.0.1:{}/", harness.host_address.port())
    );
    assert!(key.certifies(folder.worker_certificate_der()));
    assert_eq!(
        recorded,
        JoinedNode {
            node_id: expected_node_id.clone(),
            display_name: "worker".to_owned(),
            host: host(WORKER_HOST),
            manager_host: None,
            certificate_sha256: folder.worker_certificate_sha256().clone(),
            rejoined: false,
        }
    );

    let mut expected_site = harness.fixture.site.clone();
    expected_site.nodes.push(SiteNode {
        node_id: expected_node_id.clone(),
        display_name: "worker".to_owned(),
        site_manager: None,
        host: WORKER_HOST.to_owned(),
        manager_host: String::new(),
    });
    expected_site
        .execution_targets
        .extend(expected_targets(&expected_node_id));
    assert_eq!(harness.site(), expected_site);
    assert_eq!(
        harness.repository.snapshot().expect("memory"),
        expected_site
    );
    tokio::time::timeout(Duration::from_secs(10), changed.notified())
        .await
        .expect("controllers are told about the new node");
    assert_eq!(
        harness
            .live_pins
            .get(&expected_node_id)
            .map(|pin| (pin.role, pin.certificate_sha256)),
        Some((
            PrivateNodeRole::Worker,
            folder.worker_certificate_sha256().clone()
        ))
    );
    assert_eq!(
        folder.realization_targets(),
        expected_targets(&expected_node_id)
    );
    assert_eq!(
        harness.pins(),
        vec![(
            format!("{expected_node_id}.sha256"),
            format!("{}\n", folder.worker_certificate_sha256().as_str())
        )]
    );

    assert_eq!(mode(&node_root), 0o700);
    assert_eq!(mode(&node_root.join(SITE_TRUST_DIRECTORY)), 0o700);
    assert_eq!(
        mode(
            &node_root
                .join(SITE_TRUST_DIRECTORY)
                .join(NODE_PRIVATE_KEY_FILE)
        ),
        0o600
    );

    let reviews = harness.approval.reviews();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].node_id, expected_node_id);
    // The declared machine stays on the joining node; the manager reviews only the targets.
    let mut reviewed = proposal("worker", WORKER_HOST);
    reviewed.targets.declared_machine = None;
    assert_eq!(reviews[0].request, ReviewedJoin::New(reviewed));
    assert_eq!(reviews[0].host, host(WORKER_HOST));
    assert_eq!(reviews[0].host_source, JoinHostSource::Proposed);
    assert_eq!(reviews[0].manager_host, SiteNodeHost::LOOPBACK);
    assert_eq!(reviews[0].csr_key_fingerprint, key.fingerprint());
    assert_eq!(
        reviews[0].peer_address.ip(),
        std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
    );
    // The proposed name resolves to the loopback address the node connects from.
    assert!(!reviews[0].host_differs_from_peer);

    // The manager received only the signing request; it carries no private key material.
    let private_key = fs::read(
        node_root
            .join(SITE_TRUST_DIRECTORY)
            .join(NODE_PRIVATE_KEY_FILE),
    )
    .expect("private key");
    let scalar = private_scalar(&private_key);
    for csr in harness.site_writer.signed_requests() {
        assert!(!contains(&csr, &private_key));
        assert!(!contains(&csr, scalar));
    }
}

#[tokio::test]
async fn a_joined_gpu_node_offers_its_gpu_to_docker_applications() {
    let harness = Harness::start(Options::default()).await;
    let key = NodeKey::generate().expect("key");
    let node_root = harness.node_root();
    let mut connection = harness.connect().await;
    let gpu = "nvidia.com/gpu=0".to_owned();
    let gpu_node = JoinProposal {
        targets: NodeTargetChoices {
            model_targets: vec![ModelTargetChoice::Nvidia {
                selector: gpu.clone(),
                platform: ExecutionPlatform::linux_arm64(),
            }],
            application_targets: vec![ApplicationTargetChoice {
                applications: InitialApplicationRuntime::Docker,
                platform: ExecutionPlatform::linux_arm64(),
                accelerator_device_ids: vec![gpu.clone()],
            }],
            accelerator_facts: vec![AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: "NVIDIA GB10".to_owned(),
                device_id: "GPU-0".to_owned(),
                vendor_index: Some(0),
                ..AcceleratorFacts::default()
            }],
            declared_machine: None,
        },
        ..proposal("gpu-worker", WORKER_HOST)
    };

    connection
        .join(&gpu_node, &key, &node_root)
        .await
        .expect("join");
    harness.finish().await.expect("host result");

    let node_id = derive_site_node_id(&harness.fixture.site.site_id, "gpu-worker");
    let application_devices = harness
        .site()
        .execution_targets
        .into_iter()
        .filter(|target| {
            target.site_node_id == node_id
                && matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                )
        })
        .map(|target| target.accelerator_device_ids)
        .collect::<Vec<_>>();
    assert_eq!(application_devices, [vec![gpu]]);
}

#[tokio::test]
async fn a_split_network_join_records_both_addresses() {
    // The node dials the manager at another address than it declares, and the manager sees the
    // node at a source address the node never proposed.
    const DIALED: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
    const SOURCE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);
    if !loopback_addresses_usable(&[DIALED, SOURCE]) {
        return;
    }
    let harness = Harness::start(Options {
        reserved_listener_port: true,
        ..Options::default()
    })
    .await;
    start_source_forwarder(
        SocketAddr::from((DIALED, harness.host_address.port())),
        SOURCE,
        harness.host_address,
    )
    .await;
    // The manager dials the node back at its observed address, which the network forwards to
    // where the node listens with the same port, as a NAT would.
    start_source_forwarder(
        SocketAddr::from((SOURCE, worker_port())),
        DIALED,
        SocketAddr::from((route_address_toward(DIALED), worker_port())),
    )
    .await;
    let node_root = harness.node_root();
    let mut observed = proposal("worker", WORKER_HOST);
    observed.host = None;

    let joined = JoinConnection::connect(
        harness.token.clone(),
        Some(ipv4_host(DIALED)),
        Timing::test().join,
    )
    .await
    .expect("connect at the dialed manager address")
    .join(&observed, &NodeKey::generate().expect("key"), &node_root)
    .await
    .expect("split join");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(joined.host, ipv4_host(SOURCE));
    // With no proposed host, the node listens on the route to the manager before the manager
    // observes its source address, so the network must forward the recorded host to it.
    assert_eq!(
        joined.private_bind_address.to_string(),
        format!("{}:{}", route_address_toward(DIALED), worker_port())
    );
    assert_eq!(
        (recorded.host, recorded.manager_host),
        (ipv4_host(SOURCE), Some(ipv4_host(DIALED)))
    );
    let node = harness
        .site()
        .nodes
        .into_iter()
        .find(|node| node.node_id == joined.node_id)
        .expect("recorded node");
    assert_eq!(
        (node.host.as_str(), node.manager_host.as_str()),
        ("127.0.0.3", "127.0.0.2")
    );
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("valid node folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://127.0.0.2:{}/", harness.host_address.port())
    );
    assert_eq!(folder.private_bind_address(), &joined.private_bind_address);

    let reviews = harness.approval.reviews();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].host, ipv4_host(SOURCE));
    assert_eq!(reviews[0].host_source, JoinHostSource::Observed);
    assert_eq!(reviews[0].manager_host, ipv4_host(DIALED));
    assert_eq!(reviews[0].peer_address.ip(), std::net::IpAddr::V4(SOURCE));
    assert!(!reviews[0].host_differs_from_peer);
}

#[tokio::test]
async fn an_explicit_local_host_is_recorded_as_given_and_bound() {
    const HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 5);
    if !loopback_addresses_usable(&[HOST]) {
        return;
    }
    let harness = Harness::start(Options::default()).await;

    let joined = harness
        .connect()
        .await
        .join(
            &proposal("worker", "127.0.0.5"),
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect("join");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(joined.host, ipv4_host(HOST));
    assert_eq!(
        joined.private_bind_address.to_string(),
        format!("127.0.0.5:{}", worker_port())
    );
    assert_eq!(
        (recorded.host, recorded.manager_host),
        (ipv4_host(HOST), None)
    );
}

#[tokio::test]
async fn a_token_naming_the_declared_manager_host_name_records_no_manager_host() {
    let harness = Harness::start(Options {
        manager_host: Some("localhost"),
        ..Options::default()
    })
    .await;
    let node_root = harness.node_root();
    let mut connection = harness.connect().await;
    assert_eq!(connection.manager_host(), &host("localhost"));
    assert_eq!(
        connection.endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );

    let joined = connection
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect("join by host name");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(recorded.manager_host, None);
    let node = harness
        .site()
        .nodes
        .into_iter()
        .find(|node| node.node_id == joined.node_id)
        .expect("recorded node");
    assert_eq!(node.manager_host, "");
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("valid node folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );
    assert_eq!(
        harness.approval.reviews()[0].manager_host,
        host("localhost")
    );
}

#[tokio::test]
async fn a_manager_host_name_other_than_the_declared_address_is_recorded() {
    let harness = Harness::start(Options::default()).await;
    let node_root = harness.node_root();

    let joined = harness
        .connect_at(Some("LocalHost."))
        .await
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect("join at the manager host name");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(recorded.manager_host, Some(host("localhost")));
    let node = harness
        .site()
        .nodes
        .into_iter()
        .find(|node| node.node_id == joined.node_id)
        .expect("recorded node");
    assert_eq!(
        (node.host.as_str(), node.manager_host.as_str()),
        (WORKER_HOST, "localhost")
    );
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("valid node folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );
}

#[tokio::test]
async fn a_proposed_host_name_is_recorded_as_given_and_bound_by_name() {
    let resolver =
        Arc::new(worker_resolver().with("gpu-box.example.ts.net", [Ipv4Addr::LOCALHOST]));
    let harness = Harness::start(Options {
        resolver: Some(resolver),
        ..Options::default()
    })
    .await;
    let node_root = harness.node_root();

    let joined = harness
        .connect()
        .await
        .join(
            &proposal("worker", "gpu-box.example.ts.net"),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect("join with a host name");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(recorded.host, host("gpu-box.example.ts.net"));
    assert_eq!(recorded.manager_host, None);
    let bind = format!("gpu-box.example.ts.net:{}", worker_port());
    assert_eq!(joined.private_bind_address.to_string(), bind);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("valid node folder");
    assert_eq!(folder.private_bind_address().to_string(), bind);
    let node = harness
        .site()
        .nodes
        .into_iter()
        .find(|node| node.node_id == joined.node_id)
        .expect("recorded node");
    assert_eq!(node.host, "gpu-box.example.ts.net");
    let reviews = harness.approval.reviews();
    assert_eq!(reviews[0].host, host("gpu-box.example.ts.net"));
    // The manager resolves the name to the connection's own loopback source.
    assert!(!reviews[0].host_differs_from_peer);
}

#[tokio::test]
async fn an_observed_loopback_host_is_recorded_when_the_manager_reaches_the_node_there() {
    // The manager is declared by name, so the node's observed loopback address is not its host.
    let harness = Harness::start(Options {
        manager_host: Some("localhost"),
        ..Options::default()
    })
    .await;
    let mut observed = proposal("worker", WORKER_HOST);
    observed.host = None;

    let joined = harness
        .connect()
        .await
        .join(
            &observed,
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect("observed loopback host");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(recorded.host, SiteNodeHost::LOOPBACK);
    assert_eq!(
        joined.private_bind_address.to_string(),
        format!("127.0.0.1:{}", worker_port())
    );
    assert_eq!(harness.approval.reviews()[0].host, SiteNodeHost::LOOPBACK);
    assert_eq!(
        harness.approval.reviews()[0].host_source,
        JoinHostSource::Observed
    );
}

#[tokio::test]
async fn wrong_secrets_are_refused_like_a_closed_port_and_never_end_the_join() {
    let harness = Harness::start(Options::default()).await;
    let site_before = harness.site_bytes();
    let mut wrong = harness.token.clone();
    wrong.secret = JoinSecret::from_bytes([9; JOIN_SECRET_BYTES]);
    let node_root = harness.node_root();

    // Well past any attempt limit: anyone reaching the port can send these.
    for attempt in 0..12 {
        let mut connection = JoinConnection::connect(wrong.clone(), None, Timing::test().join)
            .await
            .expect("the host is authentic even with a wrong secret");
        let error = connection
            .join(
                &proposal("worker", WORKER_HOST),
                &NodeKey::generate().expect("key"),
                &node_root,
            )
            .await
            .expect_err("wrong secret");
        assert!(
            matches!(
                &error,
                NodeJoinError::Join(status)
                    if status.code() == Code::Unauthenticated
                        && status.message() == "private peer authentication failed"
            ),
            "attempt {attempt}: {error:?}"
        );
    }
    assert!(!node_root.exists());
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());
    assert!(harness.approval.reviews().is_empty());
    assert_eq!(
        harness.last_attempt(),
        None,
        "a wrong secret is no attempt of the join"
    );

    let mut connection = JoinConnection::connect(harness.token.clone(), None, Timing::test().join)
        .await
        .expect("connect with the right secret");
    let joined = connection
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect("the join still accepts its own secret");
    assert_eq!(
        harness.finish().await.expect("committed").node_id,
        joined.node_id
    );
}

#[tokio::test]
async fn tokens_that_do_not_pin_the_host_fail_before_the_secret_is_sent() {
    let harness = Harness::start(Options::default()).await;
    let other = CertificateFingerprint::from_bytes([7; 32]);
    let mutations: [(TokenEdit, &str); 3] = [
        (
            |token, other| token.site_ca_sha256 = other.clone(),
            "did not present the token's site CA certificate",
        ),
        (
            |token, _| token.manager_node_id = "node-elsewhere".to_owned(),
            "does not name the token's site manager",
        ),
        (
            |token, other| token.manager_certificate_sha256 = other.clone(),
            "does not match the token's manager pin",
        ),
    ];
    for (mutate, reason) in mutations {
        let mut token = harness.token.clone();
        mutate(&mut token, &other);
        assert_connect_refused(token, reason).await;
    }

    // Another node of the same site presents a valid site certificate, but not the pinned one.
    let impostor = ImpostorNode::start(&harness.fixture).await;
    let mut token = harness.token.clone();
    token.endpoint = impostor.endpoint();
    assert_connect_refused(token, "does not match the token's manager pin").await;

    assert!(harness.approval.reviews().is_empty());
    assert!(harness.site_writer.signed_requests().is_empty());
    let mut connection = harness.connect().await;
    connection
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect("the authentic host still accepts the join");
    harness.finish().await.expect("host result");
}

#[tokio::test]
async fn rejected_proposals_change_nothing_and_a_corrected_retry_succeeds() {
    let harness = Harness::start(Options {
        decisions: vec![JoinDecision::Reject],
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();
    let manager = harness.fixture.site.nodes[0].clone();
    let manager_host = manager.host.clone();
    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");
    let mut connection = harness.connect().await;

    let cases = [
        (
            "operator rejection",
            proposal("worker", WORKER_HOST),
            Rejection::Join,
            Code::PermissionDenied,
            "rejected",
        ),
        (
            "duplicate name",
            proposal(&manager.display_name, WORKER_HOST),
            Rejection::DuplicateName,
            Code::AlreadyExists,
            "a node named",
        ),
        (
            "duplicate host",
            proposal("worker", &manager_host),
            Rejection::DuplicateHost,
            Code::AlreadyExists,
            "is already used by node",
        ),
    ];
    for (label, proposal, expected, code, hint) in cases {
        let error = connection
            .join(&proposal, &key, &node_root)
            .await
            .expect_err(label);
        let (rejection, status) = match &error {
            NodeJoinError::Join(status) => (Rejection::Join, status),
            NodeJoinError::DuplicateName(status) => (Rejection::DuplicateName, status),
            NodeJoinError::DuplicateHost(status) => (Rejection::DuplicateHost, status),
            other => panic!("{label}: {other:?}"),
        };
        assert_eq!(rejection, expected, "{label}");
        assert_eq!(status.code(), code, "{label}");
        assert!(status.message().contains(hint), "{label}: {status:?}");
        assert!(!node_root.exists(), "{label}");
        assert_eq!(
            fs::read_dir(node_root.parent().expect("parent"))
                .expect("client directory")
                .count(),
            0,
            "{label}"
        );
        assert_eq!(harness.site_bytes(), site_before, "{label}");
        assert!(harness.pins().is_empty(), "{label}");
        let expected_attempt = match expected {
            Rejection::Join => JoinAttemptOutcome::Rejected,
            _ => JoinAttemptOutcome::Failed(status.message().to_owned()),
        };
        assert_eq!(harness.last_attempt(), Some(expected_attempt), "{label}");
    }
    // Only the proposal the operator rejected reached a review; the duplicate name and host were
    // refused before any dial.
    assert_eq!(harness.approval.reviews().len(), 1);

    connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect("corrected retry with the same token");
    assert_eq!(harness.finish().await.expect("host").display_name, "worker");
    assert_eq!(harness.pins().len(), 1);
}

#[tokio::test]
async fn identical_failed_attempts_are_told_apart_by_when_they_ended() {
    let harness = Harness::start(Options::default()).await;
    let manager_name = harness.fixture.site.nodes[0].display_name.clone();
    let duplicate = proposal(&manager_name, WORKER_HOST);
    let key = NodeKey::generate().expect("key");
    let node_root = harness.node_root();
    let mut connection = harness.connect().await;

    connection
        .join(&duplicate, &key, &node_root)
        .await
        .expect_err("a duplicate name");
    let first = harness.last_attempt_record().expect("first attempt");
    let advance = Duration::from_secs(1);
    harness.clock.advance(advance);
    connection
        .join(&duplicate, &key, &node_root)
        .await
        .expect_err("the same duplicate name");
    let second = harness.last_attempt_record().expect("second attempt");

    assert!(
        matches!(&first.outcome, JoinAttemptOutcome::Failed(reason) if reason.contains("a node named")),
        "{first:?}"
    );
    assert_eq!(second.outcome, first.outcome);
    assert!(
        second.at >= first.at + advance,
        "first {first:?}, second {second:?}"
    );
}

#[tokio::test]
async fn a_node_the_manager_cannot_reach_by_its_key_is_refused_and_a_reachable_retry_joins() {
    let harness = Harness::start(Options {
        timing: JoinTiming {
            connect_timeout: Duration::from_millis(300),
            ..test_timing()
        },
        limits: JoinLimits {
            max_failed_dial_backs: 10,
            ..JoinLimits::default()
        },
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();
    let mut client = harness.raw_client().await;
    let closed = reserve_port();
    let silent_port = reserve_port();
    let _silent = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, silent_port.port()))
        .expect("a listener that never answers");
    let join_port = harness.host_address.port();

    for (label, port, cause) in [
        ("nothing listens", closed.port(), "connection refused"),
        ("no answer", silent_port.port(), "timed out"),
        // The manager's own private listener answers there with the manager's key.
        ("another key", join_port, "another key answered"),
    ] {
        let status = client
            .join(harness.raw_request_to(WORKER_HOST, port))
            .await
            .expect_err(label);
        assert_eq!(status.code(), Code::FailedPrecondition, "{label}");
        assert_eq!(
            status.message(),
            format!("the manager could not reach this node at {WORKER_HOST}:{port}: {cause}"),
            "{label}"
        );
        assert_eq!(
            status
                .metadata()
                .get(super::proposal::REJECTION_METADATA_KEY)
                .and_then(|value| value.to_str().ok()),
            Some("unreachable-node"),
            "{label}"
        );
        assert_eq!(
            harness.last_attempt(),
            Some(JoinAttemptOutcome::Failed(status.message().to_owned())),
            "{label}"
        );
    }

    // The joining side adds where it listened, here its route to the manager since this machine
    // does not resolve the name.
    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");
    let mut connection = harness.connect().await;
    let error = connection
        .join(&proposal("worker", "unknown.example"), &key, &node_root)
        .await
        .expect_err("a name the manager cannot resolve");
    assert!(
        matches!(&error, NodeJoinError::UnreachableNode { listened, routed: true, .. }
            if *listened == SocketAddr::from((Ipv4Addr::LOCALHOST, worker_port()))),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the manager could not reach this node at unknown.example:{port}: {UNRESOLVED}; this \
             node listened at 127.0.0.1:{port}, so the \
             network must forward that host and port to it",
            port = worker_port()
        )
    );

    assert!(!node_root.exists());
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());
    assert!(harness.approval.reviews().is_empty());
    assert!(harness.site_writer.signed_requests().is_empty());

    connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect("a reachable host joins on the same token");
    assert_eq!(
        harness.finish().await.expect("host").host,
        host(WORKER_HOST)
    );
    assert_eq!(harness.approval.reviews().len(), 1);
}

#[tokio::test]
async fn a_join_abandoned_during_its_dial_back_still_counts_toward_the_limit() {
    let harness = Harness::start(Options {
        timing: JoinTiming {
            connect_timeout: Duration::from_secs(30),
            ..test_timing()
        },
        limits: JoinLimits {
            max_failed_dial_backs: 1,
            ..JoinLimits::default()
        },
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();
    let silent_port = reserve_port();
    let silent = TcpListener::bind((Ipv4Addr::LOCALHOST, silent_port.port()))
        .await
        .expect("a listener that never answers");
    let mut client = harness.raw_client().await;
    let request = harness.raw_request_to(WORKER_HOST, silent_port.port());
    let pending = tokio::spawn(async move { client.join(request).await });
    // The dial-back reaches the silent listener, which accepts and never answers.
    let _dialed = tokio::time::timeout(Duration::from_secs(30), silent.accept())
        .await
        .expect("the manager dials the node back")
        .expect("accept the dial-back");

    // The joining side gives up mid-dial, closing its connection.
    pending.abort();
    let _ = pending.await;
    for _ in 0..200 {
        let status = harness
            .raw_client()
            .await
            .join(harness.raw_request("worker"))
            .await
            .expect_err("the token is spent once the abandoned dial counted");
        if !status
            .message()
            .contains("another machine is joining with this token")
        {
            assert_eq!(status.message(), "this join token is no longer accepted");
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(harness.finish().await, Err(failed(TOO_MANY_DIAL_BACKS)));
    assert!(harness.approval.reviews().is_empty());
    assert_eq!(harness.site_bytes(), site_before);
}

#[tokio::test]
async fn a_signing_request_not_signed_by_its_key_is_refused_before_any_dial() {
    let harness = Harness::start(Options::default()).await;
    // A sender copies the raw node's public key, which a listener answers with, into a request
    // signed by another key.
    let copied_key = super::node_key::csr_public_key(&harness.raw_node.csr_der).expect("key");
    let mut forged = NodeKey::generate()
        .expect("sender key")
        .csr_der()
        .expect("csr");
    let own_key = super::node_key::csr_public_key(&forged).expect("own key");
    let at = forged
        .windows(own_key.len())
        .position(|window| window == own_key.as_slice())
        .expect("public key inside the request");
    forged[at..at + own_key.len()].copy_from_slice(&copied_key);
    let mut request = harness.raw_request("worker");
    request.csr_der = forged;

    let status = harness
        .raw_client()
        .await
        .join(request)
        .await
        .expect_err("a forged signing request");

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "the certificate signing request is not signed by its own key"
    );
    assert!(harness.approval.reviews().is_empty());
    assert_eq!(
        harness.last_attempt(),
        Some(JoinAttemptOutcome::Failed(status.message().to_owned()))
    );
}

#[tokio::test]
async fn failed_dial_backs_stop_the_host_at_the_limit() {
    let harness = Harness::start(Options {
        limits: JoinLimits {
            max_failed_dial_backs: 2,
            ..JoinLimits::default()
        },
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();
    let mut client = harness.raw_client().await;
    let closed = reserve_port();

    client
        .join(harness.raw_request_to(WORKER_HOST, closed.port()))
        .await
        .expect_err("first unreachable node");
    assert!(harness.approval.reviews().is_empty());
    client
        .join(harness.raw_request_to(WORKER_HOST, closed.port()))
        .await
        .expect_err("second unreachable node");

    assert_eq!(harness.finish().await, Err(failed(TOO_MANY_DIAL_BACKS)));
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());
}

#[tokio::test]
async fn a_host_name_joins_when_any_of_its_addresses_reaches_the_node() {
    // Nothing listens on the first address; the node listens on the second.
    const FIRST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 5);
    if !loopback_addresses_usable(&[FIRST]) {
        return;
    }
    let harness = Harness::start(Options {
        resolver: Some(Arc::new(
            worker_resolver().with("multi.example", [FIRST, Ipv4Addr::LOCALHOST]),
        )),
        ..Options::default()
    })
    .await;

    let joined = harness
        .connect()
        .await
        .with_host_resolver(Arc::new(
            worker_resolver().with("multi.example", [Ipv4Addr::LOCALHOST]),
        ))
        .join(
            &proposal("worker", "multi.example"),
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect("join at a name with several addresses");
    harness.finish().await.expect("host result");

    assert_eq!(joined.host, host("multi.example"));
    assert_eq!(harness.approval.reviews()[0].host, host("multi.example"));
}

#[tokio::test]
async fn a_taken_private_port_fails_the_join_before_the_manager_hears_of_it() {
    let harness = Harness::start(Options::default()).await;
    let site_before = harness.site_bytes();
    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");
    let mut connection = harness.connect().await;
    let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, worker_port()))
        .expect("take the private port");

    let error = connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect_err("the private port is taken");
    let NodeJoinError::PrivateListener { address, source } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        *address,
        SocketAddr::from((Ipv4Addr::LOCALHOST, worker_port()))
    );
    assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);
    assert_eq!(
        error.to_string(),
        format!(
            "could not listen on the node's private port {} at 127.0.0.1; nothing was sent to \
             the manager",
            worker_port()
        )
    );
    assert!(!node_root.exists());
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.approval.reviews().is_empty());
    assert!(harness.site_writer.signed_requests().is_empty());

    drop(taken);
    connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect("the untouched token joins once the port is free");
    harness.finish().await.expect("host result");
}

#[tokio::test]
async fn an_abandoned_offer_leaves_the_site_unchanged_and_frees_the_join() {
    let harness = Harness::start(Options::default()).await;
    let site_before = harness.site_bytes();
    let mut abandoned = harness.raw_client().await;
    let offer = request_offer(&mut abandoned, harness.raw_request("worker")).await;
    assert!(!offer.offer_id.is_empty());
    assert_eq!(offer.node_host, WORKER_HOST);
    assert_eq!(
        offer.manager_private_endpoint,
        format!("https://127.0.0.1:{}/", harness.host_address.port())
    );
    drop(abandoned);

    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());

    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");
    let joined = retry_while_busy(|| async {
        harness
            .connect()
            .await
            .join(&proposal("worker", WORKER_HOST), &key, &node_root)
            .await
    })
    .await
    .expect("join after the abandoned offer");
    assert_eq!(
        harness.finish().await.expect("host").node_id,
        joined.node_id
    );
}

#[tokio::test]
async fn one_offer_at_a_time_and_commits_only_on_its_own_connection() {
    let harness = Harness::start(Options::default()).await;
    let mut first = harness.raw_client().await;
    let offer = first
        .join(harness.raw_request("worker"))
        .await
        .expect("offer")
        .into_inner();

    let error = harness
        .connect()
        .await
        .join(
            &proposal("other", "10.0.0.8"),
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect_err("a second join while an offer is outstanding");
    assert!(
        matches!(&error, NodeJoinError::Join(status) if status.code() == Code::FailedPrecondition),
        "{error:?}"
    );

    let commit = |offer_id: &str| CommitRequest {
        join_secret: harness.token.secret.as_bytes().to_vec(),
        offer_id: offer_id.to_owned(),
    };
    let status = first
        .commit(commit("not-the-offer"))
        .await
        .expect_err("wrong offer id");
    assert_eq!(status.code(), Code::FailedPrecondition);
    let status = harness
        .raw_client()
        .await
        .commit(commit(&offer.offer_id))
        .await
        .expect_err("commit on another connection");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(harness.pins().is_empty());

    first
        .commit(commit(&offer.offer_id))
        .await
        .expect("commit on the offering connection");
    assert_eq!(harness.finish().await.expect("host").node_id, offer.node_id);
}

#[tokio::test]
async fn a_lost_commit_reply_keeps_the_folder_and_it_authenticates_to_the_manager() {
    let (cut, cut_requests) = watch::channel(false);
    let (cut_done, cut_acknowledged) = mpsc::channel();
    let cut_acknowledged = Mutex::new(cut_acknowledged);
    let harness = Harness::start(Options {
        after_publish: Some(Box::new(move || {
            cut.send_replace(true);
            cut_acknowledged
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(Duration::from_secs(30))
                .expect("proxy cut the connection");
        })),
        ..Options::default()
    })
    .await;
    let proxy = CuttingProxy::start(harness.host_address, cut_requests, cut_done).await;
    let mut token = harness.token.clone();
    token.endpoint = proxy;
    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");

    let error = JoinConnection::connect(token, None, Timing::test().join)
        .await
        .expect("connect through the proxy")
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect_err("the commit reply is lost");
    assert!(
        matches!(&error, NodeJoinError::CommitOutcomeUnknown { root, .. } if *root == node_root),
        "{error:?}"
    );
    let recorded = harness
        .finish()
        .await
        .expect("the manager recorded the node");

    let folder = NodeFolderValidator::validate_declared(&node_root).expect("kept folder");
    assert_eq!(folder.worker_node_id(), recorded.node_id);
    assert_eq!(
        folder.worker_certificate_sha256(),
        &recorded.certificate_sha256
    );

    let status = authenticate_to_manager(&harness.fixture.root, &folder).await;
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
}

#[tokio::test]
async fn idle_connections_close_but_reviews_in_progress_do_not() {
    let harness = Harness::start(Options {
        timing: JoinTiming {
            idle_timeout: Duration::from_millis(200),
            ..test_timing()
        },
        review_delay: Duration::from_millis(800),
        ..Options::default()
    })
    .await;
    // The held client never commits; the host closes its idle connection and frees the join.
    let mut held = harness.raw_client().await;
    let _offer = request_offer(&mut held, harness.raw_request("held")).await;

    // The worker commits right after its offer: a real join installs its node folder in between,
    // which can outlast the short idle timeout on a busy disk.
    let (mut worker, offer) = retry_while_busy(|| async {
        let mut worker = harness.raw_client().await;
        let offer = request_offer_result(&mut worker, harness.raw_request("worker"))
            .await
            .map_err(NodeJoinError::Join)?;
        Ok((worker, offer))
    })
    .await
    .expect("a review longer than the idle timeout still yields an offer");
    worker
        .commit(CommitRequest {
            join_secret: harness.token.secret.as_bytes().to_vec(),
            offer_id: offer.offer_id,
        })
        .await
        .expect("the offer's connection is still open to commit it");
    assert_eq!(harness.finish().await.expect("host").node_id, offer.node_id);
    assert_eq!(harness.site().nodes.len(), 2);
    drop(held);
}

#[tokio::test]
async fn a_review_is_cancelled_when_its_connection_closes_or_the_host_stops() {
    let harness = Harness::start(Options {
        hold_reviews: true,
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();

    let mut abandoned = harness.raw_client().await;
    let request = harness.raw_request("worker");
    let pending = tokio::spawn(async move {
        let _ = request_offer_result(&mut abandoned, request).await;
    });
    retry_until(|| !harness.approval.reviews().is_empty()).await;
    pending.abort();
    let _ = pending.await;
    harness.approval.wait_cancelled(&harness, 0).await;

    let mut held = harness.raw_client().await;
    let request = harness.raw_request("worker");
    let pending = tokio::spawn(async move { request_offer_result(&mut held, request).await });
    retry_until(|| harness.approval.reviews().len() == 2).await;
    harness
        .registry
        .cancel(&harness.join_id)
        .expect("cancel the join");
    harness.approval.wait_cancelled(&harness, 1).await;

    assert_eq!(harness.finish().await, Err(Box::new(JoinStatus::Cancelled)));
    pending
        .await
        .expect("join task")
        .expect_err("the abandoned review yields no offer");
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());
}

#[tokio::test]
async fn a_stop_during_a_commit_that_records_nothing_finishes_the_host() {
    let (entered, entered_signal) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let entered = Mutex::new(entered);
    let released = Mutex::new(released);
    let harness = Harness::start(Options {
        failing_publish: Some(Box::new(move || {
            let _ = entered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(());
            let _ = released
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(Duration::from_secs(30));
            JoinSiteError::Failed(anyhow::anyhow!("injected publication failure"))
        })),
        ..Options::default()
    })
    .await;
    let mut committing = harness.raw_client().await;
    let offer = request_offer_result(&mut committing, harness.raw_request("worker"))
        .await
        .expect("offer");
    let secret = harness.token.secret.as_bytes().to_vec();
    let commit = tokio::spawn(async move {
        committing
            .commit(CommitRequest {
                join_secret: secret,
                offer_id: offer.offer_id,
            })
            .await
    });
    tokio::task::spawn_blocking(move || {
        entered_signal
            .recv_timeout(Duration::from_secs(30))
            .expect("the commit is publishing")
    })
    .await
    .expect("wait for publication");

    harness
        .registry
        .cancel(&harness.join_id)
        .expect("cancel during the commit");
    release.send(()).expect("release the publication");

    let status = commit
        .await
        .expect("commit task")
        .expect_err("failed commit");
    assert_eq!(status.code(), Code::Aborted);
    assert_eq!(harness.finish().await, Err(Box::new(JoinStatus::Cancelled)));
    assert!(harness.pins().is_empty());
}

#[tokio::test]
async fn connections_beyond_the_limit_reach_no_join_until_one_closes() {
    let harness = Harness::start(Options {
        max_connections: 1,
        ..Options::default()
    })
    .await;
    let mut held = harness.raw_client().await;
    held.join(harness.raw_request_to(WORKER_HOST, reserve_port().port()))
        .await
        .expect_err("the held connection is open and served");
    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");

    // The connection over the limit is closed after its handshake, before any request.
    let refused =
        match JoinConnection::connect(harness.token.clone(), None, Timing::test().join).await {
            Ok(mut connection) => connection
                .join(&proposal("worker", WORKER_HOST), &key, &node_root)
                .await
                .map(|_| ()),
            Err(error) => Err(error),
        };
    assert!(refused.is_err(), "a second connection is refused");
    assert!(harness.approval.reviews().is_empty());

    drop(held);
    let mut joined = None;
    for _ in 0..200 {
        if let Ok(mut connection) =
            JoinConnection::connect(harness.token.clone(), None, Timing::test().join).await
            && let Ok(node) = connection
                .join(&proposal("worker", WORKER_HOST), &key, &node_root)
                .await
        {
            joined = Some(node);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    joined.expect("a join succeeds once the first connection closed");
    harness.finish().await.expect("host result");
}

#[tokio::test]
async fn a_commit_reply_that_does_not_arrive_in_time_keeps_the_folder() {
    let harness = Harness::start(Options {
        after_publish: Some(Box::new(|| std::thread::sleep(Duration::from_secs(2)))),
        ..Options::default()
    })
    .await;
    let node_root = harness.node_root();

    let error = harness
        .connect_with(JoinTiming {
            commit_timeout: Duration::from_millis(200),
            ..Timing::test().join
        })
        .await
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect_err("the commit reply is late");
    assert!(
        matches!(
            &error,
            NodeJoinError::CommitOutcomeUnknown { root, status }
                if *root == node_root && status.code() == Code::DeadlineExceeded
        ),
        "{error:?}"
    );
    let recorded = harness
        .finish()
        .await
        .expect("the manager recorded the node");
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("kept folder");
    assert_eq!(folder.worker_node_id(), recorded.node_id);
}

#[tokio::test]
async fn an_expired_join_ends_and_refuses_its_token() {
    let harness = Harness::start(Options {
        timing: JoinTiming {
            token_lifetime: Duration::ZERO,
            ..test_timing()
        },
        ..Options::default()
    })
    .await;

    assert_eq!(harness.finish().await, Err(Box::new(JoinStatus::Expired)));
    assert!(harness.pins().is_empty());
    let status = harness
        .raw_client()
        .await
        .join(harness.raw_request("worker"))
        .await
        .expect_err("the expired join refuses its token");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert!(matches!(
        harness.registry.approve(&harness.join_id, "any"),
        Err(JoinRegistryError::NotFound)
    ));
}

#[tokio::test]
async fn a_rejoin_with_a_new_host_keeps_the_node_and_changes_only_its_addresses() {
    let resolver =
        Arc::new(worker_resolver().with("gpu-box.example.ts.net", [Ipv4Addr::LOCALHOST]));
    let harness = rejoin_harness(
        &apple_proposal("worker", WORKER_HOST),
        Options {
            resolver: Some(resolver),
            ..Options::default()
        },
    )
    .await;
    let node_root = harness.node_root();
    let site_before = harness.site();
    let site_trust_before = harness.site_trust();
    let folder_before = node_folder_files(&node_root);
    let installed = NodeFolderValidator::validate_declared(&node_root).expect("installed folder");
    let declared = declared_node(&site_before, installed.worker_node_id());

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    assert_eq!(folder.node_id(), installed.worker_node_id());
    assert_eq!(folder.private_port(), worker_port());
    let joined = harness
        .connect_at(Some("localhost"))
        .await
        .rejoin(
            &RejoinProposal {
                host: Some(host("gpu-box.example.ts.net")),
                private_port: folder.private_port(),
            },
            &folder,
        )
        .await
        .expect("rejoin");
    drop(folder);
    let recorded = harness.finish().await.expect("host result");

    let rejoined = SiteNode {
        host: "gpu-box.example.ts.net".to_owned(),
        manager_host: "localhost".to_owned(),
        ..declared.clone()
    };
    let mut expected_site = site_before.clone();
    *expected_site
        .nodes
        .iter_mut()
        .find(|node| node.node_id == declared.node_id)
        .expect("declared node") = rejoined.clone();
    assert_eq!(harness.site(), expected_site);
    assert_eq!(harness.site_trust(), site_trust_before);
    assert_eq!(node_folder_files(&node_root), folder_before);
    assert_eq!(
        recorded,
        JoinedNode {
            node_id: declared.node_id.clone(),
            display_name: "worker".to_owned(),
            host: host("gpu-box.example.ts.net"),
            manager_host: Some(host("localhost")),
            certificate_sha256: installed.worker_certificate_sha256().clone(),
            rejoined: true,
        }
    );
    assert_eq!(
        (
            joined.node_id.as_str(),
            joined.display_name.as_str(),
            &joined.host
        ),
        (
            declared.node_id.as_str(),
            "worker",
            &host("gpu-box.example.ts.net")
        )
    );

    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    assert_eq!(
        folder.private_bind_address().to_string(),
        format!("gpu-box.example.ts.net:{}", worker_port())
    );
    assert_eq!(folder.stored_declared_machine(), None);
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );
    assert_eq!(
        folder.realization_targets(),
        installed.realization_targets()
    );
    assert_eq!(
        folder.observation_configuration(),
        installed.observation_configuration()
    );
    assert_eq!(
        folder.worker_certificate_sha256(),
        installed.worker_certificate_sha256()
    );

    let reviews = harness.approval.reviews();
    assert_eq!(reviews.len(), 1);
    assert_eq!(
        reviews[0].request,
        ReviewedJoin::Rejoin(NodeRejoin {
            declared,
            rejoined,
            installed_observation: installed.observation_configuration().clone(),
            observation: installed.observation_configuration().clone(),
            private_port: worker_port(),
            deployments: 0,
        })
    );
    assert_eq!(reviews[0].host_source, JoinHostSource::Proposed);
    assert!(!reviews[0].host_differs_from_peer);
    assert_eq!(
        reviews[0].csr_key_fingerprint,
        NodeKey::from_pkcs8(installed.worker_private_key_pkcs8_der())
            .expect("node key")
            .fingerprint()
    );
    // Only the first join had a certificate signed; the rejoin issued nothing.
    assert_eq!(harness.site_writer.signed_requests().len(), 1);
}

#[tokio::test]
async fn a_same_host_rejoin_leaves_the_site_unchanged() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let site_bytes = harness.site_bytes();
    let pins = harness.pins();
    let folder_before = node_folder_files(&node_root);

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    let joined = harness
        .connect()
        .await
        .rejoin(&rejoin_proposal(WORKER_HOST), &folder)
        .await
        .expect("same-host rejoin");
    drop(folder);
    let recorded = harness.finish().await.expect("host result");

    assert!(recorded.rejoined);
    assert_eq!(joined.host, host(WORKER_HOST));
    assert_eq!(harness.site_bytes(), site_bytes);
    assert_eq!(harness.pins(), pins);
    assert_eq!(node_folder_files(&node_root), folder_before);
    let ReviewedJoin::Rejoin(rejoin) = &harness.approval.reviews()[0].request else {
        panic!("a rejoin review");
    };
    assert!(rejoin.changes_nothing());
    NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
}

#[tokio::test]
async fn a_rejoin_without_a_host_keeps_the_declared_host_name() {
    // The declared name resolves to another address than the loopback the node connects from.
    const DECLARED: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 5);
    if !loopback_addresses_usable(&[DECLARED]) {
        return;
    }
    let resolver = Arc::new(worker_resolver().with("gpu-box.example.ts.net", [DECLARED]));
    let harness = rejoin_harness(
        &proposal("worker", "gpu-box.example.ts.net"),
        Options {
            resolver: Some(resolver),
            ..Options::default()
        },
    )
    .await;
    let node_root = harness.node_root();
    let site_bytes = harness.site_bytes();

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    let joined = harness
        .connect()
        .await
        .rejoin(
            &RejoinProposal {
                host: None,
                private_port: folder.private_port(),
            },
            &folder,
        )
        .await
        .expect("rejoin without a host");
    drop(folder);
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(joined.host, host("gpu-box.example.ts.net"));
    assert_eq!(recorded.host, host("gpu-box.example.ts.net"));
    assert_eq!(
        joined.private_bind_address.to_string(),
        format!("gpu-box.example.ts.net:{}", worker_port())
    );
    assert_eq!(harness.site_bytes(), site_bytes);
    let reviews = harness.approval.reviews();
    assert_eq!(reviews[0].host, host("gpu-box.example.ts.net"));
    assert_eq!(reviews[0].host_source, JoinHostSource::Declared);
    assert!(reviews[0].host_differs_from_peer);
    let ReviewedJoin::Rejoin(rejoin) = &reviews[0].request else {
        panic!("a rejoin review");
    };
    assert!(rejoin.changes_nothing());
}

#[tokio::test]
async fn a_rejoin_listens_on_its_new_host_and_private_port() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let site_bytes = harness.site_bytes();
    let new_port = reserve_port();
    let moved = RejoinProposal {
        host: Some(host(MOVED_WORKER_HOST)),
        private_port: new_port.port(),
    };
    let mut connection = harness.connect().await;

    // Taking the new port fails the rejoin before the manager hears of it.
    let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, new_port.port()))
        .expect("take the new port");
    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    let error = connection
        .rejoin(&moved, &folder)
        .await
        .expect_err("the new port is taken");
    assert!(
        matches!(
            &error,
            NodeJoinError::PrivateListener { address, .. }
                if *address == SocketAddr::from((Ipv4Addr::LOCALHOST, new_port.port()))
        ),
        "{error:?}"
    );
    assert_eq!(harness.site_bytes(), site_bytes);
    assert!(harness.approval.reviews().is_empty());
    drop(taken);

    // Taking the old port does not, since the rejoin listens on the new one only.
    let _old = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, worker_port()))
        .expect("take the old port");
    let joined = connection
        .rejoin(&moved, &folder)
        .await
        .expect("rejoin on the new host and port");
    drop(folder);
    harness.finish().await.expect("host result");

    let bind = format!("{MOVED_WORKER_HOST}:{}", new_port.port());
    assert_eq!(joined.private_bind_address.to_string(), bind);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    assert_eq!(folder.private_bind_address().to_string(), bind);
}

#[tokio::test]
async fn a_rejoin_the_manager_cannot_reach_is_refused_and_keeps_the_recorded_hosts() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let site_bytes = harness.site_bytes();
    let pins = harness.pins();
    let installed = NodeFolderValidator::validate_declared(&node_root).expect("installed folder");
    let key = NodeKey::from_pkcs8(installed.worker_private_key_pkcs8_der()).expect("node key");
    let certificate = installed.worker_certificate_der().to_vec();
    drop(installed);

    // A new host the manager cannot resolve.
    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    let error = harness
        .connect()
        .await
        .rejoin(&rejoin_proposal("unknown.example"), &folder)
        .await
        .expect_err("an unreachable new host");
    drop(folder);
    let NodeJoinError::UnreachableNode { status, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        status.message(),
        format!(
            "the manager could not reach this node at unknown.example:{}: {UNRESOLVED}",
            worker_port()
        )
    );

    // The declared host, where nothing answers on the proposed port.
    let closed = reserve_port();
    let status = harness
        .raw_client()
        .await
        .join(
            RejoinProposal {
                host: None,
                private_port: closed.port(),
            }
            .to_request(
                &harness.token.secret,
                &SiteNodeHost::LOOPBACK,
                key.rejoin_csr_der(&harness.token.secret)
                    .expect("rejoin csr"),
                certificate,
                NodeObservationConfiguration::default(),
            ),
        )
        .await
        .expect_err("an unreachable declared host");
    assert_eq!(
        status.message(),
        format!(
            "the manager could not reach this node at {WORKER_HOST}:{}: connection refused",
            closed.port()
        )
    );

    assert_eq!(harness.site_bytes(), site_bytes);
    assert_eq!(harness.pins(), pins);
    assert!(harness.approval.reviews().is_empty());
    assert_eq!(
        harness.last_attempt(),
        Some(JoinAttemptOutcome::Failed(status.message().to_owned()))
    );
}

#[tokio::test]
async fn a_rejoin_refreshes_an_observation_configuration_that_drifted_from_the_site() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let installed = NodeFolderValidator::validate_declared(&node_root).expect("installed folder");
    let installed_observation = installed.observation_configuration().clone();
    let refresh = installed_observation.interval_seconds + 30;
    let mut site = harness.site();
    site.hardware_observation = Some(HardwareObservationConfiguration {
        refresh_interval_seconds: Some(refresh),
    });
    harness
        .repository
        .replace(site)
        .expect("change the site observation policy");
    let site_bytes = harness.site_bytes();

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    harness
        .connect()
        .await
        .rejoin(&rejoin_proposal(WORKER_HOST), &folder)
        .await
        .expect("rejoin with a drifted observation configuration");
    drop(folder);
    assert!(harness.finish().await.expect("host result").rejoined);

    assert_eq!(harness.site_bytes(), site_bytes);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    let observation = folder.observation_configuration().clone();
    assert_eq!(observation.interval_seconds, refresh);
    assert_eq!(
        installed.stored_declared_machine(),
        Some(&simulated_rtx_5090())
    );
    assert_eq!(
        folder.stored_declared_machine(),
        Some(&simulated_rtx_5090())
    );
    assert_eq!(
        NodeObservationConfiguration {
            interval_seconds: installed_observation.interval_seconds,
            ..observation.clone()
        },
        installed_observation
    );
    assert_eq!(
        folder.realization_targets(),
        installed.realization_targets()
    );
    let ReviewedJoin::Rejoin(rejoin) = &harness.approval.reviews()[0].request else {
        panic!("a rejoin review");
    };
    assert_eq!(rejoin.declared, rejoin.rejoined);
    assert_eq!(rejoin.installed_observation, installed_observation);
    assert_eq!(rejoin.observation, observation);
    assert!(!rejoin.changes_nothing());
}

#[tokio::test]
async fn rejoin_refusals_leave_the_site_and_pins_unchanged() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let installed = NodeFolderValidator::validate_declared(&node_root).expect("installed folder");
    let node_id = installed.worker_node_id().to_owned();
    let certificate = installed.worker_certificate_der().to_vec();
    let key = NodeKey::from_pkcs8(installed.worker_private_key_pkcs8_der()).expect("node key");
    let secret = harness.token.secret.clone();
    let site_id = harness.fixture.site.site_id.clone();
    let trust = ManagerTrust::load(&harness.fixture.root, &harness.fixture.site).expect("trust");
    let site_trust = harness.fixture.root.join(SITE_TRUST_DIRECTORY);
    let manager_certificate = fs::read(site_trust.join(NODE_CERTIFICATE_FILE)).expect("manager");
    let manager_key = NodeKey::from_pkcs8(
        &fs::read(site_trust.join(NODE_PRIVATE_KEY_FILE)).expect("manager key"),
    )
    .expect("manager node key");
    let (removed_certificate, removed_key) = trust
        .issue_worker(&site_id, "node-removed")
        .expect("removed node");
    let removed_key = NodeKey::from_pkcs8(&removed_key).expect("removed node key");
    let expired = certificate_valid_between(&trust, &site_id, &node_id, &key, -2, -1);
    let not_yet_valid = certificate_valid_between(&trust, &site_id, &node_id, &key, 1, 2);
    let other_key = NodeKey::generate().expect("other key");
    let bound = |key: &NodeKey| key.rejoin_csr_der(&secret).expect("bound csr");
    let manager_host = harness.fixture.site.nodes[0].host.clone();
    let mut client = harness.raw_client().await;

    let cases: [(
        &str,
        JoinRequest,
        Option<CertificateFingerprint>,
        Code,
        &str,
    ); 8] = [
        (
            "a proof from another key",
            rejoin_request(&secret, WORKER_HOST, &certificate, bound(&other_key)),
            None,
            Code::PermissionDenied,
            "signed by another key than the node's certificate",
        ),
        (
            "a proof replayed from another join",
            rejoin_request(
                &secret,
                WORKER_HOST,
                &certificate,
                key.rejoin_csr_der(&JoinSecret::from_bytes([9; JOIN_SECRET_BYTES]))
                    .expect("csr of another join"),
            ),
            None,
            Code::PermissionDenied,
            "was not made for this join",
        ),
        (
            "a removed node",
            rejoin_request(
                &secret,
                "192.0.2.9",
                &removed_certificate,
                bound(&removed_key),
            ),
            None,
            Code::FailedPrecondition,
            "is no longer declared in this site; leave this machine's node folder",
        ),
        (
            "the manager's own node",
            rejoin_request(
                &secret,
                "192.0.2.9",
                &manager_certificate,
                bound(&manager_key),
            ),
            None,
            Code::PermissionDenied,
            "the site manager's own node cannot join again",
        ),
        (
            "a wrong pin",
            rejoin_request(&secret, WORKER_HOST, &certificate, bound(&key)),
            Some(CertificateFingerprint::from_bytes([7; 32])),
            Code::FailedPrecondition,
            "is not the one this site pins for node \"worker\"",
        ),
        (
            "an expired certificate",
            rejoin_request(&secret, WORKER_HOST, &expired, bound(&key)),
            Some(CertificateFingerprint::for_certificate(&expired)),
            Code::FailedPrecondition,
            "the certificate of node \"worker\" has expired",
        ),
        (
            "a certificate not valid yet",
            rejoin_request(&secret, WORKER_HOST, &not_yet_valid, bound(&key)),
            Some(CertificateFingerprint::for_certificate(&not_yet_valid)),
            Code::FailedPrecondition,
            "the certificate of node \"worker\" is not valid yet; check the clocks of the \
             manager and the joining node",
        ),
        (
            "another node's host",
            rejoin_request(&secret, &manager_host, &certificate, bound(&key)),
            None,
            Code::AlreadyExists,
            "is already used by node",
        ),
    ];
    let pin_path = site_trust
        .join(WORKER_PIN_DIRECTORY)
        .join(format!("{node_id}.sha256"));
    let pin = fs::read(&pin_path).expect("pin");
    for (label, request, pinned, code, message) in cases {
        if let Some(pinned) = &pinned {
            fs::write(&pin_path, format!("{}\n", pinned.as_str())).expect("replace pin");
        }
        let site_bytes = harness.site_bytes();
        let pins = harness.pins();

        let status = client.join(request).await.expect_err(label);

        assert_eq!(status.code(), code, "{label}: {status:?}");
        assert!(status.message().contains(message), "{label}: {status:?}");
        assert_eq!(harness.site_bytes(), site_bytes, "{label}");
        assert_eq!(harness.pins(), pins, "{label}");
        fs::write(&pin_path, &pin).expect("restore pin");
    }
    let reasons = [
        (
            "a removed node",
            "node-not-declared",
            &removed_certificate,
            &removed_key,
            "192.0.2.9",
        ),
        (
            "a stale certificate",
            "certificate-expired",
            &expired,
            &key,
            WORKER_HOST,
        ),
        (
            "an early certificate",
            "certificate-not-yet-valid",
            &not_yet_valid,
            &key,
            WORKER_HOST,
        ),
    ];
    for (label, reason, certificate, key, proposed_host) in reasons {
        fs::write(
            &pin_path,
            format!(
                "{}\n",
                CertificateFingerprint::for_certificate(certificate).as_str()
            ),
        )
        .expect("pin the certificate");
        let status = client
            .join(rejoin_request(
                &secret,
                proposed_host,
                certificate,
                bound(key),
            ))
            .await
            .expect_err(label);
        assert_eq!(
            status
                .metadata()
                .get(super::proposal::REJECTION_METADATA_KEY)
                .and_then(|value| value.to_str().ok()),
            Some(reason),
            "{label}"
        );
        fs::write(&pin_path, &pin).expect("restore pin");
    }

    let site_bytes = harness.site_bytes();
    let mut connection = harness.connect().await;
    let error = connection
        .join(
            &proposal("worker", "192.0.2.9"),
            &NodeKey::generate().expect("key"),
            &harness.client_directory.path().join("other-node"),
        )
        .await
        .expect_err("a declared name without a proof");
    let NodeJoinError::DuplicateName(status) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        status.message(),
        "a node named \"worker\" already exists in this site; choose another name, or remove \
         that node first if this machine replaces it"
    );
    assert_eq!(harness.site_bytes(), site_bytes);
    assert!(harness.approval.reviews().is_empty());

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    connection
        .rejoin(&rejoin_proposal(MOVED_WORKER_HOST), &folder)
        .await
        .expect("the token still admits a valid rejoin");
    drop(folder);
    assert!(harness.finish().await.expect("host result").rejoined);
}

#[tokio::test]
async fn a_failed_folder_rewrite_after_the_commit_is_completed_by_rejoining_again() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let manifest = fs::read(node_root.join(WORKER_MANIFEST_FILE)).expect("worker.yaml");

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    let error = harness
        .connect_at(Some("localhost"))
        .await
        .with_file_system(Arc::new(FailingRename))
        .rejoin(&rejoin_proposal(MOVED_WORKER_HOST), &folder)
        .await
        .expect_err("the folder rewrite fails");
    drop(folder);
    assert!(
        matches!(&error, NodeJoinError::FolderRewrite { root, .. } if *root == node_root),
        "{error:?}"
    );
    assert!(harness.finish().await.expect("host result").rejoined);
    assert_eq!(
        fs::read(node_root.join(WORKER_MANIFEST_FILE)).expect("worker.yaml"),
        manifest
    );
    let site_bytes = harness.site_bytes();
    let node = declared_node(&harness.site(), &rejoined_node_id(&harness));
    assert_eq!(
        (node.host.as_str(), node.manager_host.as_str()),
        (MOVED_WORKER_HOST, "localhost")
    );

    let harness = harness.restart(Options::default()).await;
    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    harness
        .connect_at(Some("localhost"))
        .await
        .rejoin(&rejoin_proposal(MOVED_WORKER_HOST), &folder)
        .await
        .expect("a repeated rejoin");
    drop(folder);
    harness.finish().await.expect("host result");

    assert_eq!(harness.site_bytes(), site_bytes);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );
}

#[tokio::test]
async fn an_unknown_rejoin_commit_outcome_keeps_the_folder_and_rejoining_again_completes_it() {
    let (cut, cut_requests) = watch::channel(false);
    let (cut_done, cut_acknowledged) = mpsc::channel();
    let cut_acknowledged = Mutex::new(cut_acknowledged);
    let harness = rejoin_harness(
        &proposal("worker", WORKER_HOST),
        Options {
            after_publish: Some(Box::new(move || {
                cut.send_replace(true);
                cut_acknowledged
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(30))
                    .expect("proxy cut the connection");
            })),
            ..Options::default()
        },
    )
    .await;
    let proxy = CuttingProxy::start(harness.host_address, cut_requests, cut_done).await;
    let mut token = harness.token.clone();
    token.endpoint = proxy;
    let node_root = harness.node_root();
    let manifest = fs::read(node_root.join(WORKER_MANIFEST_FILE)).expect("worker.yaml");

    let folder = RejoiningNodeFolder::open(&node_root, &token).expect("open folder");
    let error = JoinConnection::connect(token, Some(host("localhost")), Timing::test().join)
        .await
        .expect("connect through the proxy")
        .with_host_resolver(Arc::clone(&harness.resolver))
        .rejoin(&rejoin_proposal(MOVED_WORKER_HOST), &folder)
        .await
        .expect_err("the commit reply is lost");
    drop(folder);
    assert!(
        matches!(&error, NodeJoinError::CommitOutcomeUnknown { root, .. } if *root == node_root),
        "{error:?}"
    );
    assert!(
        harness
            .finish()
            .await
            .expect("the manager recorded the rejoin")
            .rejoined
    );
    assert_eq!(
        fs::read(node_root.join(WORKER_MANIFEST_FILE)).expect("worker.yaml"),
        manifest
    );
    let site_bytes = harness.site_bytes();

    let harness = harness.restart(Options::default()).await;
    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    harness
        .connect_at(Some("localhost"))
        .await
        .rejoin(&rejoin_proposal(MOVED_WORKER_HOST), &folder)
        .await
        .expect("a repeated rejoin");
    drop(folder);
    harness.finish().await.expect("host result");

    assert_eq!(harness.site_bytes(), site_bytes);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );
}

#[tokio::test]
async fn a_rewrite_that_may_not_be_durable_is_reported_as_uncertain() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();

    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    let error = harness
        .connect_at(Some("localhost"))
        .await
        .with_file_system(Arc::new(FailingDirectorySync))
        .rejoin(&rejoin_proposal(MOVED_WORKER_HOST), &folder)
        .await
        .expect_err("the folder rewrite is not confirmed");
    drop(folder);

    assert!(
        matches!(
            &error,
            NodeJoinError::FolderRewriteUncertain {
                root,
                source: NodeFolderError::RewriteUncertain(_),
            } if *root == node_root
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("may not be durable"), "{error}");
    assert!(harness.finish().await.expect("host result").rejoined);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rewritten folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        format!("https://localhost:{}/", harness.host_address.port())
    );
}

#[tokio::test]
async fn a_folder_certificate_outside_its_validity_is_refused_before_anything_is_sent() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let installed = NodeFolderValidator::validate_declared(&node_root).expect("installed folder");
    let key = NodeKey::from_pkcs8(installed.worker_private_key_pkcs8_der()).expect("node key");
    let trust = ManagerTrust::load(&harness.fixture.root, &harness.fixture.site).expect("trust");
    let site_id = harness.fixture.site.site_id.clone();
    let node_id = installed.worker_node_id().to_owned();
    let certificate_path = node_root
        .join(SITE_TRUST_DIRECTORY)
        .join(NODE_CERTIFICATE_FILE);
    let certificate = fs::read(&certificate_path).expect("node certificate");

    for (label, days, message) in [
        (
            "expired",
            (-2, -1),
            "has expired; remove the node from the site, leave this machine's node folder, then \
             join as a new node",
        ),
        (
            "not valid yet",
            (1, 2),
            "is not valid yet; check the clocks of this machine and the manager",
        ),
    ] {
        let replaced = certificate_valid_between(&trust, &site_id, &node_id, &key, days.0, days.1);
        fs::write(&certificate_path, replaced).expect("replace node certificate");

        let error = RejoiningNodeFolder::open(&node_root, &harness.token)
            .err()
            .expect(label);

        assert!(
            matches!(
                &error,
                NodeFolderError::CertificateExpired { root }
                    | NodeFolderError::CertificateNotYetValid { root }
                    if *root == node_root
            ),
            "{label}: {error:?}"
        );
        assert!(error.to_string().contains(message), "{label}: {error}");
    }
    fs::write(&certificate_path, certificate).expect("restore node certificate");
    RejoiningNodeFolder::open(&node_root, &harness.token).expect("a valid folder opens");
    assert!(harness.approval.reviews().is_empty());
}

#[tokio::test]
async fn a_folder_that_cannot_rejoin_is_refused_before_anything_is_sent() {
    let harness = rejoin_harness(&proposal("worker", WORKER_HOST), Options::default()).await;
    let node_root = harness.node_root();
    let folder_before = node_folder_files(&node_root);
    let other = CertificateFingerprint::from_bytes([7; 32]);
    let token_edits: [(TokenEdit, &str); 3] = [
        (
            |token, _| token.site_id = "site-other".to_owned(),
            "belongs to another site",
        ),
        (
            |token, other| token.site_ca_sha256 = other.clone(),
            "belongs to an earlier trust of this site",
        ),
        (
            |token, other| token.manager_certificate_sha256 = other.clone(),
            "belongs to an earlier trust of this site",
        ),
    ];
    for (edit, message) in token_edits {
        let mut token = harness.token.clone();
        edit(&mut token, &other);
        let error = RejoiningNodeFolder::open(&node_root, &token)
            .err()
            .expect("refused folder");
        assert!(error.to_string().contains(message), "{message}: {error}");
    }

    let running = WorkerNodeCustody::try_acquire(&node_root).expect("node custody");
    let error = RejoiningNodeFolder::open(&node_root, &harness.token)
        .err()
        .expect("running node");
    assert!(
        matches!(&error, NodeFolderError::NodeRunning { root } if *root == node_root),
        "{error:?}"
    );
    drop(running);

    assert_eq!(node_folder_files(&node_root), folder_before);
    assert!(harness.approval.reviews().is_empty());
}

#[tokio::test]
async fn a_failed_or_uncertain_site_write_fails_the_commit_and_records_nothing() {
    for fault in [SiteWriteFault::Storage, SiteWriteFault::Uncertain] {
        let harness = Harness::start(Options {
            write_faults: vec![fault],
            ..Options::default()
        })
        .await;
        let site_bytes = harness.site_bytes();
        let memory = harness.repository.snapshot().expect("memory");
        let site_trust = harness.site_trust();
        let changed = harness.repository.register_change_listener();
        let node_root = harness.node_root();
        let key = NodeKey::generate().expect("key");

        let error = harness
            .connect()
            .await
            .join(&proposal("worker", WORKER_HOST), &key, &node_root)
            .await
            .expect_err("the commit fails");

        assert!(
            matches!(&error, NodeJoinError::CommitRejected(status)
                if status.code() == Code::Aborted
                    && status.message() == "the manager could not record the node and recorded nothing"),
            "{fault:?}: {error:?}"
        );
        assert!(!node_root.exists(), "{fault:?}");
        assert_eq!(harness.site_bytes(), site_bytes, "{fault:?}");
        assert_eq!(
            harness.site_trust(),
            site_trust,
            "{fault:?}: no pin is left"
        );
        assert_eq!(
            harness.repository.snapshot().expect("memory"),
            memory,
            "{fault:?}"
        );
        assert!(!harness.repository.is_dirty(), "{fault:?}");
        let node_id = derive_site_node_id(&harness.fixture.site.site_id, "worker");
        assert!(harness.live_pins.get(&node_id).is_none(), "{fault:?}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), changed.notified())
                .await
                .is_err(),
            "{fault:?}: controllers hear of no change"
        );
        assert!(
            matches!(harness.last_attempt(), Some(JoinAttemptOutcome::Failed(reason))
                if reason.contains("recorded nothing")),
            "{fault:?}"
        );

        // A manager restarting now loads the site as it was and starts its private listener.
        let reloaded = SiteRepository::load(
            &harness.fixture.root.join(SITE_FILE),
            crate::test_support::site_declaration_rules(),
        )
        .expect("the site loads after the failed commit");
        assert_eq!(reloaded.snapshot().expect("reloaded"), memory, "{fault:?}");
        validate_for_private_startup(&harness.fixture.root, &memory)
            .expect("the manager identity still validates");

        // The token stays usable for a retry.
        harness
            .connect()
            .await
            .join(&proposal("worker", WORKER_HOST), &key, &node_root)
            .await
            .expect("a retry joins");
        assert_eq!(harness.finish().await.expect("host").node_id, node_id);
    }
}

#[tokio::test]
async fn concurrent_joins_with_the_same_name_or_host_sign_only_one_node() {
    let harness = Harness::start(Options {
        hold_reviews: true,
        ..Options::default()
    })
    .await;
    let second = harness.registry.start().await.expect("a second join");
    let second_request = |display_name: &str, proposed_host: &str| {
        JoinProposal {
            private_port: harness.raw_node.port.port(),
            ..proposal(display_name, proposed_host)
        }
        .to_request(
            &second.token.secret,
            &SiteNodeHost::LOOPBACK,
            harness.raw_node.csr_der.clone(),
        )
    };
    // The first join's proposal waits for the operator and reserves its name and host.
    let mut held = harness.raw_client().await;
    let request = harness.raw_request("worker");
    let pending = tokio::spawn(async move { request_offer_result(&mut held, request).await });
    retry_until(|| !harness.approval.reviews().is_empty()).await;

    let mut client = harness.raw_client().await;
    for (label, display_name, proposed_host, reason) in [
        ("same name", "worker", MOVED_WORKER_HOST, "duplicate-name"),
        ("same host", "other", WORKER_HOST, "duplicate-host"),
    ] {
        let status = client
            .join(second_request(display_name, proposed_host))
            .await
            .expect_err(label);
        assert_eq!(status.code(), Code::AlreadyExists, "{label}: {status:?}");
        assert_eq!(
            status
                .metadata()
                .get(super::proposal::REJECTION_METADATA_KEY)
                .and_then(|value| value.to_str().ok()),
            Some(reason),
            "{label}"
        );
    }

    let proposal_id = match harness.status() {
        JoinStatus::AwaitingApproval { proposal_id, .. } => proposal_id,
        other => panic!("{other:?}"),
    };
    harness
        .registry
        .approve(&harness.join_id, &proposal_id)
        .expect("approve the first join");
    let offer = pending.await.expect("join task").expect("offer");
    assert_eq!(harness.site_writer.signed_requests().len(), 1);
    assert_eq!(offer.node_display_name, "worker");
}

#[tokio::test]
async fn the_first_decision_wins_and_no_proposal_is_decided_before_its_dial_back() {
    let harness = Harness::start(Options {
        hold_reviews: true,
        ..Options::default()
    })
    .await;
    assert!(matches!(
        harness.registry.approve(&harness.join_id, "guessed"),
        Err(JoinRegistryError::StaleProposal)
    ));

    // A proposal whose dial-back fails never awaits a decision.
    let mut client = harness.raw_client().await;
    client
        .join(harness.raw_request_to(WORKER_HOST, reserve_port().port()))
        .await
        .expect_err("an unreachable node");
    assert!(harness.approval.reviews().is_empty());
    assert!(matches!(
        harness.last_attempt(),
        Some(JoinAttemptOutcome::Failed(_))
    ));

    let request = harness.raw_request("worker");
    let pending = tokio::spawn(async move { request_offer_result(&mut client, request).await });
    retry_until(|| !harness.approval.reviews().is_empty()).await;
    let proposal_id = match harness.status() {
        JoinStatus::AwaitingApproval { proposal_id, .. } => proposal_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        harness.registry.approve(&harness.join_id, "stale"),
        Err(JoinRegistryError::StaleProposal)
    ));
    assert!(matches!(
        harness.registry.approve("unknown-join", &proposal_id),
        Err(JoinRegistryError::NotFound)
    ));

    harness
        .registry
        .reject(&harness.join_id, &proposal_id)
        .expect("the first decision");
    assert!(matches!(
        harness.registry.approve(&harness.join_id, &proposal_id),
        Err(JoinRegistryError::AlreadyDecided)
    ));
    assert!(matches!(
        harness.registry.reject(&harness.join_id, &proposal_id),
        Err(JoinRegistryError::AlreadyDecided)
    ));
    let status = pending
        .await
        .expect("join task")
        .expect_err("the rejected proposal");
    assert_eq!(status.code(), Code::PermissionDenied);
    assert_eq!(harness.last_attempt(), Some(JoinAttemptOutcome::Rejected));
    assert!(harness.site_writer.signed_requests().is_empty());
}

#[tokio::test]
async fn a_cancelled_join_refuses_its_token_and_stays_readable_until_forgotten() {
    let clock = ManualClock::new();
    let (_fixture, registry) = bare_registry(Arc::new(clock.clone()));
    let started = registry.start().await.expect("start");
    let mut watch = registry.watch(&started.join_id).expect("watch");
    assert_eq!(watch.status(), JoinStatus::Waiting { last_attempt: None });

    registry.cancel(&started.join_id).expect("cancel");
    assert_eq!(watch.status(), JoinStatus::Cancelled);
    assert!(matches!(
        registry.cancel(&started.join_id),
        Err(JoinRegistryError::NotFound)
    ));
    assert!(matches!(
        registry.approve(&started.join_id, "any"),
        Err(JoinRegistryError::NotFound)
    ));
    let status = registry
        .shared_for_test()
        .join_for_secret(
            &PrivateConnectionInfo::unattached("127.0.0.1:50000".parse().expect("peer")),
            started.token.secret.as_bytes(),
        )
        .err()
        .expect("the cancelled token is refused");
    assert_eq!(status.code(), Code::Unauthenticated);

    // An ended join stays readable, then is forgotten.
    assert_eq!(
        registry
            .watch(&started.join_id)
            .expect("ended join")
            .status(),
        JoinStatus::Cancelled
    );
    drop(watch);
    let retention = test_timing().ended_retention;
    retry_until_async(|| {
        clock.advance(retention);
        let forgotten = matches!(
            registry.watch(&started.join_id),
            Err(JoinRegistryError::NotFound)
        );
        async move { forgotten }
    })
    .await;
}

#[tokio::test]
async fn a_join_nobody_follows_is_cancelled_after_the_grace_period() {
    let clock = ManualClock::new();
    let (_fixture, registry) = bare_registry(Arc::new(clock.clone()));
    let grace = test_timing().watcher_grace;
    let followed = registry.start().await.expect("followed join");
    let unfollowed = registry.start().await.expect("unfollowed join");
    let mut watch = registry.watch(&followed.join_id).expect("watch");

    // A watch that closes starts the grace period again from its end.
    let mut left = registry.watch(&unfollowed.join_id).expect("watch");
    drop(left);
    retry_until_async(|| {
        clock.advance(grace);
        let status = registry
            .watch(&unfollowed.join_id)
            .map(|mut watch| watch.status());
        async move { matches!(status, Ok(JoinStatus::Cancelled)) }
    })
    .await;
    // Reading a join counts as following it only while the watch lives.
    left = registry.watch(&unfollowed.join_id).expect("ended join");
    assert_eq!(left.status(), JoinStatus::Cancelled);

    clock.advance(grace * 3);
    tokio::task::yield_now().await;
    assert_eq!(watch.status(), JoinStatus::Waiting { last_attempt: None });
}

#[tokio::test]
async fn at_most_the_active_join_limit_runs_at_once() {
    let (_fixture, registry) = bare_registry(Arc::new(SystemClock));
    let limit = JoinLimits::default().max_active_joins;
    let mut joins = Vec::new();
    for _ in 0..limit {
        joins.push(registry.start().await.expect("a join under the limit"));
    }
    let _watches = joins
        .iter()
        .map(|join| registry.watch(&join.join_id).expect("watch"))
        .collect::<Vec<_>>();

    assert!(matches!(
        registry.start().await,
        Err(JoinRegistryError::TooManyJoins)
    ));
    registry.cancel(&joins[0].join_id).expect("cancel one");
    registry.start().await.expect("a join once another ended");
}

#[tokio::test]
async fn the_management_api_starts_follows_and_decides_joins() {
    use dotcore_proto::dotcore::site::v1::{
        self as proto, site_membership_service_server::SiteMembershipService as _,
    };
    use futures_util::StreamExt as _;

    let harness = Harness::start(Options {
        hold_reviews: true,
        ..Options::default()
    })
    .await;
    let clock = ManualClock::new();
    let heartbeat = Duration::from_secs(15);
    let service = crate::membership::MembershipManagementService::new(
        harness.registry.clone(),
        crate::membership::MachineRemoval::new(
            harness.repository.clone(),
            &harness.fixture.root,
            harness.live_pins.clone(),
            Arc::new(crate::membership::test_support::Disconnected),
        ),
        Arc::new(clock.clone()),
        heartbeat,
    );

    // A started join hands out a token naming the manager's private listener.
    let before = std::time::SystemTime::now();
    let started = service
        .start_join(tonic::Request::new(proto::StartJoinRequest {}))
        .await
        .expect("start a join")
        .into_inner();
    let token = JoinToken::parse(&started.token).expect("token");
    assert_eq!(
        token.endpoint.as_str(),
        format!("https://127.0.0.1:{}/", harness.host_address.port())
    );
    let expires = started.expires_at.expect("expiry").seconds;
    let lifetime = i64::try_from(test_timing().token_lifetime.as_secs()).expect("lifetime");
    let since_epoch = |time: std::time::SystemTime| {
        i64::try_from(
            time.duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
        )
        .expect("seconds")
    };
    assert!(
        (since_epoch(before) + lifetime..=since_epoch(before) + lifetime + 5).contains(&expires)
    );
    service
        .cancel_join(tonic::Request::new(proto::CancelJoinRequest {
            join_id: started.join_id.clone(),
        }))
        .await
        .expect("cancel the started join");
    let status = service
        .cancel_join(tonic::Request::new(proto::CancelJoinRequest {
            join_id: started.join_id,
        }))
        .await
        .expect_err("the cancelled join ended");
    assert_eq!(status.code(), Code::NotFound);

    // The watch opens on the current state and sends a heartbeat when quiet.
    let mut stream = service
        .watch_join(tonic::Request::new(proto::WatchJoinRequest {
            join_id: harness.join_id.clone(),
        }))
        .await
        .expect("watch the join")
        .into_inner();
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .expect("a watch message")
            .map(|message| message.expect("watch message").event.expect("event"))
    };
    assert_eq!(
        next().await,
        Some(proto::watch_join_response::Event::State(proto::JoinState {
            state: Some(proto::join_state::State::Waiting(proto::JoinWaiting {
                last_attempt: None
            })),
        }))
    );
    let quiet = tokio::spawn({
        let clock = clock.clone();
        async move {
            loop {
                clock.advance(heartbeat);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    });
    assert_eq!(
        next().await,
        Some(proto::watch_join_response::Event::Heartbeat(
            Default::default()
        ))
    );
    quiet.abort();

    // A proposal reaches the watch with what the operator decides on.
    let key = NodeKey::generate().expect("key");
    let key_fingerprint = key.fingerprint();
    let node_root = harness.node_root();
    let mut connection = harness.connect().await;
    let joining = tokio::spawn(async move {
        connection
            .join(&proposal("worker", WORKER_HOST), &key, &node_root)
            .await
    });
    let awaiting = loop {
        match next().await.expect("an event") {
            proto::watch_join_response::Event::State(proto::JoinState {
                state: Some(proto::join_state::State::AwaitingApproval(awaiting)),
            }) => break awaiting,
            _ => continue,
        }
    };
    let review = harness.approval.reviews()[0].clone();
    let node_id = derive_site_node_id(&harness.fixture.site.site_id, "worker");
    let expected_targets = expected_targets(&node_id);
    assert_eq!(
        awaiting.proposal,
        Some(proto::JoinProposal {
            node_id: node_id.clone(),
            display_name: "worker".to_owned(),
            host: WORKER_HOST.to_owned(),
            host_source: proto::JoinHostSource::Proposed as i32,
            peer_address: review.peer_address.to_string(),
            host_differs_from_peer: false,
            private_port: u32::from(worker_port()),
            key_fingerprint: key_fingerprint.as_str().to_owned(),
            fingerprint_pattern: Some(expected_pattern(&key_fingerprint)),
            kind: Some(proto::join_proposal::Kind::NewNode(proto::JoinNewNode {
                targets: vec![
                    proto::JoinProposedTarget {
                        workload_kind: proto::ExecutionTargetWorkloadKind::Model as i32,
                        runtime: proto::JoinTargetRuntime::Simulated as i32,
                        platform_label: "linux/amd64".to_owned(),
                        accelerator_device_ids: expected_targets[0].accelerator_device_ids.clone(),
                        native_workload_lifetime: proto::NativeWorkloadLifetime::Unspecified as i32,
                    },
                    proto::JoinProposedTarget {
                        workload_kind: proto::ExecutionTargetWorkloadKind::Application as i32,
                        runtime: proto::JoinTargetRuntime::Simulated as i32,
                        platform_label: "linux/arm64".to_owned(),
                        accelerator_device_ids: Vec::new(),
                        native_workload_lifetime: proto::NativeWorkloadLifetime::Unspecified as i32,
                    },
                ],
            })),
        })
    );

    let decide = |proposal_id: &str| {
        tonic::Request::new(proto::ApproveJoinRequest {
            join_id: harness.join_id.clone(),
            proposal_id: proposal_id.to_owned(),
        })
    };
    let stale = service
        .approve_join(decide("stale"))
        .await
        .expect_err("a stale proposal");
    assert_eq!(stale.code(), Code::FailedPrecondition);
    service
        .approve_join(decide(&awaiting.proposal_id))
        .await
        .expect("approve");
    let again = service
        .approve_join(decide(&awaiting.proposal_id))
        .await
        .expect_err("a second decision");
    assert!(
        matches!(again.code(), Code::FailedPrecondition | Code::NotFound),
        "{again:?}"
    );

    // The watch ends with the committed node.
    let committed = loop {
        match next().await.expect("an event") {
            proto::watch_join_response::Event::State(proto::JoinState {
                state: Some(proto::join_state::State::Committed(committed)),
            }) => break committed,
            _ => continue,
        }
    };
    assert_eq!(committed.node_id, node_id);
    assert_eq!(next().await, None);
    joining.await.expect("join task").expect("joined");
}

/// The wire form of the pattern derived from `fingerprint`.
fn expected_pattern(
    fingerprint: &CertificateFingerprint,
) -> dotcore_proto::dotcore::site::v1::FingerprintPattern {
    use dotcore_proto::dotcore::site::v1::FingerprintPatternColor as Color;

    let pattern = FingerprintPattern::of(fingerprint);
    dotcore_proto::dotcore::site::v1::FingerprintPattern {
        colors: pattern
            .colors
            .iter()
            .map(|color| {
                (match color {
                    FingerprintColor::Red => Color::Red,
                    FingerprintColor::Green => Color::Green,
                    FingerprintColor::Yellow => Color::Yellow,
                    FingerprintColor::Blue => Color::Blue,
                    FingerprintColor::Magenta => Color::Magenta,
                    FingerprintColor::Cyan => Color::Cyan,
                }) as i32
            })
            .collect(),
        cells: pattern
            .cells
            .iter()
            .flatten()
            .map(|&slot| u32::from(slot))
            .collect(),
    }
}

/// A registry over an initialized site on `clock`, serving no listener.
fn bare_registry(clock: Arc<dyn dotcore_domain::clock::Clock>) -> (InitializedSite, JoinRegistry) {
    let fixture = initialized_site();
    let repository = SiteRepository::load(
        &fixture.root.join(SITE_FILE),
        crate::test_support::site_declaration_rules(),
    )
    .expect("site repository");
    let site = repository.snapshot().expect("site");
    let identity = validate_for_private_startup(&fixture.root, &site).expect("manager identity");
    let manager = site
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .expect("manager");
    let registry = JoinRegistry::new(
        JoinHostIdentity::new(
            &site.site_id,
            &manager.node_id,
            identity.trust_anchor,
            identity.certificate,
        ),
        Arc::new(MembershipRecorder::new(
            repository,
            &fixture.root,
            PeerPins::default(),
        )),
        JoinGate::default(),
        test_timing(),
        clock,
    );
    registry.listen_on(7443);
    (fixture, registry)
}

/// Waits until `condition` resolves true, polling briefly.
async fn retry_until_async<F: std::future::Future<Output = bool>>(
    mut condition: impl FnMut() -> F,
) {
    for _ in 0..1200 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition did not hold");
}

/// Why a join fails after too many failed dial-backs.
const TOO_MANY_DIAL_BACKS: &str =
    "too many joins named a host and port where the manager could not reach the machine";

/// The final state of a join that failed for `reason`.
fn failed(reason: &str) -> Box<JoinStatus> {
    Box::new(JoinStatus::Failed {
        reason: reason.to_owned(),
    })
}

/// Which refusal variant a join error is.
#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    Join,
    DuplicateName,
    DuplicateHost,
}

/// Changes one pinned value of a token.
type TokenEdit = fn(&mut JoinToken, &CertificateFingerprint);

/// Proposed worker host, a name both sides resolve to this machine's loopback address, where the
/// joining node listens and the manager reaches it without any loopback alias.
const WORKER_HOST: &str = "worker.example";

/// Why a dial-back to `unknown.example` fails, with the fake resolver's cause.
const UNRESOLVED: &str = "the name does not resolve on the manager's machine: could not resolve \
                          host name unknown.example; check that the network providing it is up: \
                          unknown.example is unknown";

/// Host a worker moves to when it joins again, resolved like [`WORKER_HOST`].
const MOVED_WORKER_HOST: &str = "worker-moved.example";

thread_local! {
    /// Private port joining nodes of the running test listen on, held for the whole test. Each
    /// test runs on its own thread or process, so tests never share it.
    static WORKER_PORT: ReservedPort = reserve_port();
}

/// Returns the private port joining nodes of the running test propose and listen on.
fn worker_port() -> u16 {
    WORKER_PORT.with(ReservedPort::port)
}

/// Resolver of the worker host names, and of `localhost` as the system resolves it, for both the
/// joining node and the join host; tests add the names they need.
fn worker_resolver() -> FakeHostResolver {
    FakeHostResolver::default()
        .with(WORKER_HOST, [Ipv4Addr::LOCALHOST])
        .with(MOVED_WORKER_HOST, [Ipv4Addr::LOCALHOST])
        .with("localhost", [Ipv4Addr::LOCALHOST])
}

fn proposal(display_name: &str, proposed_host: &str) -> JoinProposal {
    JoinProposal {
        display_name: display_name.to_owned(),
        host: Some(host(proposed_host)),
        private_port: worker_port(),
        targets: simulated_targets(),
    }
}

fn host(value: &str) -> SiteNodeHost {
    SiteNodeHost::parse(value).expect("host")
}

fn ipv4_host(address: Ipv4Addr) -> SiteNodeHost {
    SiteNodeHost::from_ipv4(address).expect("specific address")
}

fn expected_targets(node_id: &str) -> Vec<ExecutionTarget> {
    crate::initial::inputs::build_execution_targets(node_id, &simulated_targets())
}

/// Simulated models on the GPU of a declared RTX 5090 desktop, on linux/amd64, and simulated
/// applications on linux/arm64.
fn simulated_targets() -> NodeTargetChoices {
    NodeTargetChoices {
        model_targets: vec![ModelTargetChoice::Simulated {
            platform: ExecutionPlatform::linux_amd64(),
            device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
        }],
        application_targets: vec![ApplicationTargetChoice {
            applications: InitialApplicationRuntime::Simulated,
            platform: ExecutionPlatform::linux_arm64(),
            accelerator_device_ids: Vec::new(),
        }],
        accelerator_facts: Vec::new(),
        declared_machine: Some(simulated_rtx_5090()),
    }
}

/// The hardware a simulated node declares by default: one RTX 5090 on the simulated host.
fn simulated_rtx_5090() -> DeclaredMachine {
    DeclaredMachine {
        choices: vec!["rtx-5090".to_owned()],
        description: Some(
            MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
                .expect("simulated machine")
                .to_stored(),
        ),
    }
}

/// Test join bounds.
fn test_timing() -> JoinTiming {
    Timing::test().join
}

struct Options {
    limits: JoinLimits,
    /// Connections without a pinned certificate the listener keeps open at once.
    max_connections: usize,
    timing: JoinTiming,
    /// Host the site declares for the manager and the token names; the listener's loopback
    /// address when `None`.
    manager_host: Option<&'static str>,
    /// Resolver both sides look host names up with; [`worker_resolver`] when `None`.
    resolver: Option<Arc<FakeHostResolver>>,
    /// Binds the private listener on a reserved port, for tests that bind the same number on
    /// another address.
    reserved_listener_port: bool,
    decisions: Vec<JoinDecision>,
    review_delay: Duration,
    hold_reviews: bool,
    after_publish: Option<PublishHook>,
    failing_publish: Option<FailingPublish>,
    /// Outcomes of the next `site.yaml` writes.
    write_faults: Vec<SiteWriteFault>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limits: JoinLimits::default(),
            max_connections: 16,
            timing: test_timing(),
            manager_host: None,
            resolver: None,
            reserved_listener_port: false,
            decisions: Vec::new(),
            review_delay: Duration::ZERO,
            hold_reviews: false,
            after_publish: None,
            failing_publish: None,
            write_faults: Vec::new(),
        }
    }
}

/// A join registry served on a private listener over an initialized site, with one started join
/// and a scripted operator.
struct Harness {
    fixture: InitializedSite,
    registry: JoinRegistry,
    /// Clock of the registry: real time plus the test's advances.
    clock: ManualClock,
    join_id: String,
    token: JoinToken,
    host_address: SocketAddr,
    /// Keeps the listener's number from other test processes while the harness lives.
    _reserved_port: Option<ReservedPort>,
    approval: Arc<ApprovalScript>,
    site_writer: Arc<RecordingSite>,
    repository: SiteRepository,
    live_pins: PeerPins,
    resolver: Arc<dyn HostResolver>,
    client_directory: tempfile::TempDir,
    raw_node: RawNode,
    _server: AbortOnDrop,
    _operator: AbortOnDrop,
}

/// The joining node behind raw requests: its signing request, and its dial-back listener on a
/// private port of its own, so raw requests pass the dial-back next to the harness's real joins.
struct RawNode {
    csr_der: Vec<u8>,
    port: ReservedPort,
    _listener: DialBackListener,
}

impl RawNode {
    async fn start() -> Self {
        let key = NodeKey::generate().expect("raw node key");
        let port = reserve_port();
        let listener =
            DialBackListener::start(SocketAddr::from((Ipv4Addr::LOCALHOST, port.port())), &key)
                .await
                .expect("raw node dial-back listener");
        Self {
            csr_der: key.csr_der().expect("raw node csr"),
            port,
            _listener: listener,
        }
    }
}

impl Harness {
    async fn start(options: Options) -> Self {
        let fixture = match options.manager_host {
            Some(host) => initialized_site_with_manager_host(host),
            None => initialized_site(),
        };
        let (repository, faults) =
            SiteRepository::load_with_write_faults_for_test(&fixture.root.join(SITE_FILE))
                .expect("site repository");
        let live_pins = PeerPins::default();
        let site_writer = Arc::new(RecordingSite {
            inner: MembershipRecorder::new(repository.clone(), &fixture.root, live_pins.clone()),
            faults,
            after_publish: Mutex::default(),
            failing_publish: Mutex::default(),
            signed_requests: Mutex::default(),
        });
        let client_directory =
            dotcore_domain::test_support::canonical_tempdir().expect("client directory");
        Self::serve(
            fixture,
            site_writer,
            repository,
            live_pins,
            client_directory,
            options,
        )
        .await
    }

    /// Serves a new registry with a fresh join over the same site and joining machine, as a
    /// later join on the same manager would. The previous join must have ended.
    async fn restart(self, options: Options) -> Self {
        let Self {
            fixture,
            site_writer,
            repository,
            live_pins,
            client_directory,
            ..
        } = self;
        Self::serve(
            fixture,
            site_writer,
            repository,
            live_pins,
            client_directory,
            options,
        )
        .await
    }

    async fn serve(
        fixture: InitializedSite,
        site_writer: Arc<RecordingSite>,
        repository: SiteRepository,
        live_pins: PeerPins,
        client_directory: tempfile::TempDir,
        options: Options,
    ) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        site_writer.set_hooks(options.after_publish, options.failing_publish);
        for fault in options.write_faults {
            site_writer.faults.push(fault);
        }
        let site = repository.snapshot().expect("site");
        let identity =
            validate_for_private_startup(&fixture.root, &site).expect("manager identity");
        let manager = site
            .nodes
            .iter()
            .find(|node| node.site_manager.is_some())
            .expect("manager node")
            .clone();
        let configuration = PrivateServerConfiguration {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            site_id: site.site_id.clone(),
            local_node_id: manager.node_id.clone(),
            local_role: PrivateNodeRole::Manager,
            certificate_der: identity.certificate.clone(),
            private_key_pkcs8: identity.private_key,
            trust_anchor_der: identity.trust_anchor.clone(),
            peers: live_pins.clone(),
        };
        let reserved_port = options.reserved_listener_port.then(reserve_port);
        let listener = TcpListener::bind((
            Ipv4Addr::LOCALHOST,
            reserved_port.as_ref().map_or(0, ReservedPort::port),
        ))
        .await
        .expect("private listener");
        let host_address = listener.local_addr().expect("listener address");
        let resolver: Arc<dyn HostResolver> = match options.resolver {
            Some(resolver) => resolver,
            None => Arc::new(worker_resolver()),
        };
        let clock = ManualClock::new();
        let registry = JoinRegistry::new(
            JoinHostIdentity::new(
                &site.site_id,
                &manager.node_id,
                identity.trust_anchor,
                identity.certificate,
            ),
            Arc::clone(&site_writer) as Arc<dyn JoinSiteWriter>,
            JoinGate::default(),
            options.timing,
            Arc::new(clock.clone()),
        )
        .with_host_resolver(Arc::clone(&resolver))
        .with_limits(options.limits);
        registry.listen_on(host_address.port());
        let incoming = PrivateConnections::new(
            &configuration,
            UnpinnedConnectionLimits {
                max_open: options.max_connections,
                idle_timeout: options.timing.idle_timeout,
            },
        )
        .expect("private connections")
        .incoming(listener);
        let server = tonic::transport::Server::builder()
            .add_service(registry.join_service())
            .serve_with_incoming(incoming);
        let server = AbortOnDrop(tokio::spawn(async move {
            let _ = server.await;
        }));

        let started = registry.start().await.expect("start a join");
        let approval = Arc::new(ApprovalScript {
            delay: options.review_delay,
            hold: options.hold_reviews,
            decisions: Mutex::new(options.decisions.into()),
            reviews: Mutex::default(),
        });
        let operator = AbortOnDrop(tokio::spawn(operate(
            registry.clone(),
            started.join_id.clone(),
            Arc::clone(&approval),
        )));
        Self {
            fixture,
            registry,
            clock,
            join_id: started.join_id,
            token: started.token,
            host_address,
            _reserved_port: reserved_port,
            approval,
            site_writer,
            repository,
            live_pins,
            resolver,
            client_directory,
            raw_node: RawNode::start().await,
            _server: server,
            _operator: operator,
        }
    }

    /// A join request of the raw node, proposing `display_name` at [`WORKER_HOST`].
    fn raw_request(&self, display_name: &str) -> JoinRequest {
        self.raw_request_with(&self.token, display_name)
    }

    /// A join request of the raw node proposing `proposed_host` and `private_port`, where nothing
    /// may answer with its key.
    fn raw_request_to(&self, proposed_host: &str, private_port: u16) -> JoinRequest {
        JoinProposal {
            private_port,
            ..proposal("worker", proposed_host)
        }
        .to_request(
            &self.token.secret,
            &SiteNodeHost::LOOPBACK,
            self.raw_node.csr_der.clone(),
        )
    }

    /// Like [`Self::raw_request`], with the secret of `token`.
    fn raw_request_with(&self, token: &JoinToken, display_name: &str) -> JoinRequest {
        JoinProposal {
            private_port: self.raw_node.port.port(),
            ..proposal(display_name, WORKER_HOST)
        }
        .to_request(
            &token.secret,
            &SiteNodeHost::LOOPBACK,
            self.raw_node.csr_der.clone(),
        )
    }

    async fn connect(&self) -> JoinConnection {
        self.connect_at(None).await
    }

    /// Connects with `timing` instead of the production join bounds.
    async fn connect_with(&self, timing: JoinTiming) -> JoinConnection {
        JoinConnection::connect(self.token.clone(), None, timing)
            .await
            .expect("connect to the manager")
            .with_host_resolver(Arc::clone(&self.resolver))
    }

    /// Connects at `manager_host` instead of the token's host when given.
    async fn connect_at(&self, manager_host: Option<&str>) -> JoinConnection {
        JoinConnection::connect(
            self.token.clone(),
            manager_host.map(|value| SiteNodeHost::normalize(value).expect("manager host")),
            Timing::test().join,
        )
        .await
        .expect("connect to the manager")
        .with_host_resolver(Arc::clone(&self.resolver))
    }

    /// A generated client on its own verified connection, for steps the joining node never
    /// takes on its own.
    async fn raw_client(&self) -> NodeJoinServiceClient<Channel> {
        let verifier = Arc::new(JoinHostVerifier::new(&self.token));
        let channel = Endpoint::from_shared(self.token.endpoint.to_string())
            .expect("endpoint")
            .tls_config_with_verifier(
                ClientTlsConfig::new()
                    .domain_name("127.0.0.1")
                    .assume_http2(true),
                verifier,
            )
            .expect("TLS")
            .connect()
            .await
            .expect("raw connection");
        NodeJoinServiceClient::new(channel)
    }

    /// Waits for the join to end and returns the recorded node, or the state it ended in.
    async fn finish(&self) -> Result<JoinedNode, Box<JoinStatus>> {
        let mut watch = self.registry.watch(&self.join_id).expect("watch the join");
        let mut status = watch.status();
        let ended = tokio::time::timeout(Duration::from_secs(60), async {
            while !status.is_final() {
                status = watch.changed().await;
            }
            status
        })
        .await
        .expect("the join ends");
        match ended {
            JoinStatus::Committed(joined) => Ok(joined),
            other => Err(Box::new(other)),
        }
    }

    /// Returns the join's current state.
    fn status(&self) -> JoinStatus {
        self.registry
            .watch(&self.join_id)
            .expect("watch the join")
            .status()
    }

    /// Returns how the join's latest attempt ended, failing when it is not waiting for one.
    fn last_attempt(&self) -> Option<JoinAttemptOutcome> {
        self.last_attempt_record().map(|attempt| attempt.outcome)
    }

    /// Returns the join's latest attempt, failing when it is not waiting for one.
    fn last_attempt_record(&self) -> Option<JoinAttempt> {
        match self.status() {
            JoinStatus::Waiting { last_attempt } => last_attempt,
            other => panic!("the join is not waiting: {other:?}"),
        }
    }

    fn node_root(&self) -> PathBuf {
        self.client_directory.path().join("node")
    }

    fn site(&self) -> Site {
        load_validated_site(
            &self.fixture.root.join(SITE_FILE),
            &crate::test_support::site_declaration_rules(),
        )
        .expect("Site")
    }

    fn site_bytes(&self) -> Vec<u8> {
        fs::read(self.fixture.root.join(SITE_FILE)).expect("site.yaml")
    }

    /// Every file of the site trust, including the pins, with its content.
    fn site_trust(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        directory_files(&self.fixture.root.join(SITE_TRUST_DIRECTORY))
    }

    /// Every pin file with its content.
    fn pins(&self) -> Vec<(String, String)> {
        let directory = self
            .fixture
            .root
            .join(SITE_TRUST_DIRECTORY)
            .join(WORKER_PIN_DIRECTORY);
        let mut pins = fs::read_dir(&directory)
            .expect("pin directory")
            .map(|entry| {
                let entry = entry.expect("pin entry");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read_to_string(entry.path()).expect("pin"),
                )
            })
            .collect::<Vec<_>>();
        pins.sort();
        pins
    }
}

/// Aborts a background task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Follows one join as the operator and decides each proposal from the script.
async fn operate(registry: JoinRegistry, join_id: String, script: Arc<ApprovalScript>) {
    let mut watch = registry.watch(&join_id).expect("watch the join");
    let mut status = watch.status();
    while !status.is_final() {
        if let JoinStatus::AwaitingApproval {
            proposal_id,
            review,
        } = &status
        {
            script
                .reviews
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((proposal_id.clone(), (**review).clone()));
            if !script.hold {
                tokio::time::sleep(script.delay).await;
                let decision = script
                    .decisions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pop_front()
                    .unwrap_or(JoinDecision::Allow);
                let _ = match decision {
                    JoinDecision::Allow => registry.approve(&join_id, proposal_id),
                    JoinDecision::Reject => registry.reject(&join_id, proposal_id),
                };
            }
        }
        status = watch.changed().await;
    }
}

/// The operator's scripted answers, and the proposals it saw.
struct ApprovalScript {
    delay: Duration,
    hold: bool,
    decisions: Mutex<VecDeque<JoinDecision>>,
    reviews: Mutex<Vec<(String, JoinReview)>>,
}

impl ApprovalScript {
    fn reviews(&self) -> Vec<JoinReview> {
        self.reviews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, review)| review.clone())
            .collect()
    }

    /// Waits until the proposal at `index` no longer awaits a decision.
    async fn wait_cancelled(&self, harness: &Harness, index: usize) {
        let proposal = self.reviews.lock().unwrap_or_else(PoisonError::into_inner)[index]
            .0
            .clone();
        retry_until(|| {
            !matches!(
                harness.status(),
                JoinStatus::AwaitingApproval { proposal_id, .. } if proposal_id == proposal
            )
        })
        .await;
    }
}

/// The membership recorder, recording every signing request and running a hook after each write
/// of a node; the hooks are replaced whenever a new registry serves the site.
struct RecordingSite {
    inner: MembershipRecorder,
    faults: SiteWriteFaults,
    after_publish: Mutex<Option<PublishHook>>,
    failing_publish: Mutex<Option<FailingPublish>>,
    signed_requests: Mutex<Vec<Vec<u8>>>,
}

impl RecordingSite {
    fn signed_requests(&self) -> Vec<Vec<u8>> {
        self.signed_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_hooks(
        &self,
        after_publish: Option<PublishHook>,
        failing_publish: Option<FailingPublish>,
    ) {
        *self
            .after_publish
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = after_publish;
        *self
            .failing_publish
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = failing_publish;
    }

    /// Runs the failure hook if any, then `write`, then the hook after a write.
    fn write_with_hooks(
        &self,
        write: impl FnOnce() -> Result<(), JoinSiteError>,
    ) -> Result<(), JoinSiteError> {
        if let Some(failure) = &*self
            .failing_publish
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(failure());
        }
        write()?;
        if let Some(hook) = &*self
            .after_publish
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            hook();
        }
        Ok(())
    }
}

/// Runs after the site recorded a node, for example to cut the connection before the reply.
type PublishHook = Box<dyn Fn() + Send + Sync>;

/// Fails a node write instead of recording it.
type FailingPublish = Box<dyn Fn() -> JoinSiteError + Send + Sync>;

impl JoinSiteWriter for RecordingSite {
    fn declaration_rules(&self) -> crate::validation::SiteDeclarationRules {
        self.inner.declaration_rules()
    }

    fn site(&self) -> Result<Site, JoinSiteError> {
        self.inner.site()
    }

    fn sign_node_certificate(
        &self,
        site: &Site,
        node: &SiteNode,
        csr_der: &[u8],
    ) -> Result<Vec<u8>, JoinSiteError> {
        self.signed_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(csr_der.to_vec());
        self.inner.sign_node_certificate(site, node, csr_der)
    }

    fn publish_node(
        &self,
        node: &SiteNode,
        targets: &[ExecutionTarget],
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError> {
        self.write_with_hooks(|| self.inner.publish_node(node, targets, certificate_der))
    }

    fn node_pin(&self, node_id: &str) -> Result<Option<CertificateFingerprint>, JoinSiteError> {
        self.inner.node_pin(node_id)
    }

    fn replace_node_hosts(
        &self,
        node: &SiteNode,
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError> {
        self.write_with_hooks(|| self.inner.replace_node_hosts(node, certificate_der))
    }
}

/// A TLS server presenting another valid node certificate of the same site, with the site CA.
struct ImpostorNode {
    address: SocketAddr,
    _server: JoinHandle<()>,
}

impl ImpostorNode {
    async fn start(fixture: &InitializedSite) -> Self {
        let trust = ManagerTrust::load(&fixture.root, &fixture.site).expect("Site trust");
        let (certificate, key) = trust
            .issue_worker(&fixture.site.site_id, "node-impostor")
            .expect("impostor leaf");
        let tls = ServerTlsConfig::new().identity(Identity::from_pem(
            format!(
                "{}{}",
                pem("CERTIFICATE", &certificate),
                pem("CERTIFICATE", &trust.trust_anchor)
            ),
            pem("PRIVATE KEY", &key),
        ));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("impostor listener");
        let address = listener.local_addr().expect("impostor address");
        let server = tonic::transport::Server::builder()
            .tls_config(tls)
            .expect("impostor TLS")
            .add_routes(tonic::service::Routes::builder().routes())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener));
        Self {
            address,
            _server: tokio::spawn(async move {
                let _ = server.await;
            }),
        }
    }

    fn endpoint(&self) -> url::Url {
        format!("https://{}", self.address)
            .parse()
            .expect("impostor endpoint")
    }
}

/// A TCP proxy that forwards to the join host until told to cut every connection, so the host's
/// reply to an already processed request never arrives.
struct CuttingProxy;

impl CuttingProxy {
    async fn start(
        target: SocketAddr,
        cut: watch::Receiver<bool>,
        cut_done: mpsc::Sender<()>,
    ) -> url::Url {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("proxy listener");
        let address = listener.local_addr().expect("proxy address");
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let mut cut = cut.clone();
                let cut_done = cut_done.clone();
                tokio::spawn(async move {
                    let mut outbound = tokio::net::TcpStream::connect(target)
                        .await
                        .expect("proxy target");
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = cut.wait_for(|cut| *cut) => {
                            drop(inbound);
                            drop(outbound);
                            let _ = cut_done.send(());
                        }
                    }
                });
            }
        });
        format!("https://{address}")
            .parse()
            .expect("proxy endpoint")
    }
}

/// Forwards connections accepted at `listen` to `target` from the local address `source`, so the
/// target observes `source` as the peer, as a node on another network would appear.
async fn start_source_forwarder(listen: SocketAddr, source: Ipv4Addr, target: SocketAddr) {
    let listener = TcpListener::bind(listen).await.expect("forwarder listener");
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            tokio::spawn(async move {
                let socket = tokio::net::TcpSocket::new_v4().expect("forwarder socket");
                socket
                    .bind(SocketAddr::from((source, 0)))
                    .expect("simulated source address");
                let mut outbound = socket.connect(target).await.expect("forwarder target");
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
}

/// Returns the local address this machine connects from toward `destination`.
fn route_address_toward(destination: Ipv4Addr) -> Ipv4Addr {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("route socket");
    // Connecting a UDP socket sends nothing; it only selects the route and its local address.
    socket.connect((destination, 9)).expect("route");
    match socket.local_addr().expect("route address") {
        SocketAddr::V4(address) => *address.ip(),
        SocketAddr::V6(address) => panic!("an IPv6 route to {destination}: {address}"),
    }
}

/// Reports whether every loopback address can be bound. Linux routes all of 127.0.0.0/8 to the
/// loopback interface; macOS needs an `lo0` alias per address, for example
/// `sudo ifconfig lo0 alias 127.0.0.2`.
fn loopback_addresses_usable(addresses: &[Ipv4Addr]) -> bool {
    let usable = addresses
        .iter()
        .all(|address| std::net::TcpListener::bind((*address, 0)).is_ok());
    if !usable {
        eprintln!("skipped: loopback addresses {addresses:?} are not usable on this machine");
    }
    usable
}

/// Runs the manager's private listener from the site on disk and calls it with the joined
/// folder's identity. Ingress authentication passes, so the call reaches routing and fails as
/// unimplemented.
async fn authenticate_to_manager(
    site_root: &Path,
    folder: &crate::node::folder::ValidatedNodeFolder,
) -> tonic::Status {
    let site = load_validated_site(
        &site_root.join(SITE_FILE),
        &crate::test_support::site_declaration_rules(),
    )
    .expect("Site");
    let identity = validate_for_private_startup(site_root, &site).expect("manager identity");
    let manager = site
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .expect("manager");
    let pin = fs::read_to_string(
        site_root
            .join(SITE_TRUST_DIRECTORY)
            .join(WORKER_PIN_DIRECTORY)
            .join(format!("{}.sha256", folder.worker_node_id())),
    )
    .expect("pin");
    let configuration = PrivateServerConfiguration {
        bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        site_id: site.site_id.clone(),
        local_node_id: manager.node_id.clone(),
        local_role: PrivateNodeRole::Manager,
        certificate_der: identity.certificate.clone(),
        private_key_pkcs8: identity.private_key,
        trust_anchor_der: identity.trust_anchor.clone(),
        peers: PeerPins::new(
            [(
                folder.worker_node_id().to_owned(),
                PrivatePeerPolicy {
                    role: PrivateNodeRole::Worker,
                    certificate_sha256: CertificateFingerprint::parse(pin.trim())
                        .expect("pin value"),
                },
            )]
            .into(),
        ),
    };
    assert_eq!(
        CertificateFingerprint::parse(pin.trim()).expect("pin value"),
        CertificateFingerprint::for_certificate(folder.worker_certificate_der()),
        "the manager pins the worker's issued certificate"
    );
    let server = crate::test_support::private_tls::MutualTlsListener::start(
        &configuration,
        tonic::service::Routes::builder().routes(),
    )
    .await;
    let address = server.address;

    let verifier = Arc::new(
        PinnedPeerVerifier::new(
            folder.trust_anchor_der(),
            folder.site_id(),
            folder.manager_node_id(),
            folder.manager_certificate_sha256().clone(),
        )
        .expect("manager verifier"),
    );
    let channel = Endpoint::from_shared(format!("https://{address}"))
        .expect("endpoint")
        .tls_config_with_verifier(
            ClientTlsConfig::new()
                .domain_name(manager.host.clone())
                .identity(Identity::from_pem(
                    pem("CERTIFICATE", folder.worker_certificate_der()),
                    pem("PRIVATE KEY", folder.worker_private_key_pkcs8_der()),
                ))
                .assume_http2(true),
            verifier,
        )
        .expect("TLS")
        .connect()
        .await
        .expect("mutual TLS with the manager");
    NodeJoinServiceClient::new(channel)
        .commit(CommitRequest::default())
        .await
        .expect_err("the manager serves no join service")
}

async fn assert_connect_refused(token: JoinToken, reason: &str) {
    let error = JoinConnection::connect(token, None, Timing::test().join)
        .await
        .err()
        .unwrap_or_else(|| panic!("{reason}: the host must not be accepted"));
    let NodeJoinError::Connect(source) = &error else {
        panic!("{reason}: {error:?}");
    };
    assert!(
        format!("{source:#}").contains(reason),
        "{reason}: {source:#}"
    );
}

async fn request_offer(
    client: &mut NodeJoinServiceClient<Channel>,
    request: JoinRequest,
) -> JoinResponse {
    request_offer_result(client, request).await.expect("offer")
}

async fn request_offer_result(
    client: &mut NodeJoinServiceClient<Channel>,
    request: JoinRequest,
) -> Result<JoinResponse, tonic::Status> {
    client.join(request).await.map(tonic::Response::into_inner)
}

/// Waits until `condition` holds, polling briefly.
async fn retry_until(condition: impl Fn() -> bool) {
    for _ in 0..1200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition did not hold");
}

/// Retries while the host still holds the slot of a connection that is closing.
async fn retry_while_busy<T, F>(mut attempt: impl FnMut() -> F) -> Result<T, NodeJoinError>
where
    F: std::future::Future<Output = Result<T, NodeJoinError>>,
{
    for _ in 0..200 {
        match attempt().await {
            Err(NodeJoinError::Join(status)) if status.code() == Code::FailedPrecondition => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            result => return result,
        }
    }
    attempt().await
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// The 32-byte private scalar inside a P-256 PKCS#8 key (`ECPrivateKey` version 1 then an
/// octet string of 32 bytes).
fn private_scalar(pkcs8: &[u8]) -> &[u8] {
    let marker = [0x02, 0x01, 0x01, 0x04, 0x20];
    let start = pkcs8
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("ECPrivateKey scalar")
        + marker.len();
    &pkcs8[start..start + 32]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Joins `first` through a first host, then serves a second host with a fresh token over the
/// same site, so the joined node can join again.
async fn rejoin_harness(first: &JoinProposal, options: Options) -> Harness {
    let harness = Harness::start(Options {
        resolver: options.resolver.clone(),
        ..Options::default()
    })
    .await;
    harness
        .connect()
        .await
        .join(
            first,
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect("first join");
    harness.finish().await.expect("first host result");
    harness.restart(options).await
}

fn rejoin_proposal(proposed_host: &str) -> RejoinProposal {
    RejoinProposal {
        host: Some(host(proposed_host)),
        private_port: worker_port(),
    }
}

/// A rejoin request for `certificate`, proposing `proposed_host`, with the signing request `csr`.
fn rejoin_request(
    secret: &JoinSecret,
    proposed_host: &str,
    certificate: &[u8],
    csr: Vec<u8>,
) -> JoinRequest {
    rejoin_proposal(proposed_host).to_request(
        secret,
        &SiteNodeHost::LOOPBACK,
        csr,
        certificate.to_vec(),
        NodeObservationConfiguration::default(),
    )
}

/// A node with Apple inference on darwin/arm64 and Docker applications on linux/arm64.
fn apple_proposal(display_name: &str, proposed_host: &str) -> JoinProposal {
    JoinProposal {
        targets: NodeTargetChoices {
            model_targets: vec![ModelTargetChoice::Apple {
                device_id: "apple-gpu-0".to_owned(),
                lifetime: NativeWorkloadLifetime::Ephemeral,
            }],
            application_targets: vec![ApplicationTargetChoice {
                applications: InitialApplicationRuntime::Docker,
                platform: ExecutionPlatform::linux_arm64(),
                accelerator_device_ids: Vec::new(),
            }],
            accelerator_facts: Vec::new(),
            declared_machine: None,
        },
        ..proposal(display_name, proposed_host)
    }
}

fn declared_node(site: &Site, node_id: &str) -> SiteNode {
    site.nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .expect("declared node")
        .clone()
}

fn rejoined_node_id(harness: &Harness) -> String {
    NodeFolderValidator::validate_declared(&harness.node_root())
        .expect("node folder")
        .worker_node_id()
        .to_owned()
}

/// Every file of a node folder with its content, except its `worker.yaml`.
fn node_folder_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = directory_files(root);
    files.remove(Path::new(WORKER_MANIFEST_FILE));
    files
}

/// Every file under `root`, by its path relative to `root`, with its content.
fn directory_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).expect("directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                directories.push(path);
            } else {
                let relative = path.strip_prefix(root).expect("relative path").to_owned();
                files.insert(relative, fs::read(&path).expect("file"));
            }
        }
    }
    files
}

/// A file system whose replacing rename always fails, as a full or failing disk would.
struct FailingRename;

impl FileSystem for FailingRename {
    fn create_temp(&self, path: &Path) -> std::io::Result<(PathBuf, fs::File)> {
        StdFileSystem.create_temp(path)
    }

    fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> std::io::Result<()> {
        StdFileSystem.write_all(file, bytes)
    }

    fn sync_file(&self, file: &fs::File) -> std::io::Result<()> {
        StdFileSystem.sync_file(file)
    }

    fn rename(&self, _from: &Path, _to: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("injected rename failure"))
    }

    fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        StdFileSystem.sync_directory(path)
    }
}

/// A certificate for `node_id` and `key`, signed by the site CA and valid from `from_days` to
/// `to_days` days from now.
fn certificate_valid_between(
    trust: &ManagerTrust,
    site_id: &str,
    node_id: &str,
    key: &NodeKey,
    from_days: i64,
    to_days: i64,
) -> Vec<u8> {
    let now = time::OffsetDateTime::now_utc();
    trust
        .sign_csr_valid_between(
            site_id,
            node_id,
            &key.csr_der().expect("csr"),
            now + time::Duration::days(from_days),
            now + time::Duration::days(to_days),
        )
        .expect("certificate")
}

/// A file system whose directory sync after the replacing rename fails, so the replacement is
/// done but not confirmed durable.
struct FailingDirectorySync;

impl FileSystem for FailingDirectorySync {
    fn create_temp(&self, path: &Path) -> std::io::Result<(PathBuf, fs::File)> {
        StdFileSystem.create_temp(path)
    }

    fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> std::io::Result<()> {
        StdFileSystem.write_all(file, bytes)
    }

    fn sync_file(&self, file: &fs::File) -> std::io::Result<()> {
        StdFileSystem.sync_file(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        StdFileSystem.rename(from, to)
    }

    fn sync_directory(&self, _path: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("injected directory sync failure"))
    }
}
