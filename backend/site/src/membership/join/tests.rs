//! End-to-end node join tests: the join host and the joining node talk over loopback TLS with
//! real site certificates, against an initialized offline site on disk.

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
    ExecutionPlatform, HostResolver, SiteNodeHost, SystemHostResolver,
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
use futures_util::future::BoxFuture;
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tonic::{
    Code,
    transport::{Channel, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig},
};

use super::{
    JOIN_SECRET_BYTES, JoinApproval, JoinConnection, JoinDecision, JoinHost, JoinHostError,
    JoinHostLimits, JoinHostSettings, JoinHostSource, JoinProposal, JoinReview, JoinSecret,
    JoinSiteError, JoinSiteWriter, JoinToken, JoinedNode, NodeFolderError, NodeJoinError, NodeKey,
    NodeRejoin, RejoinProposal, RejoiningNodeFolder, ReviewedJoin,
    test_fixtures::{InitializedSite, initialized_site, initialized_site_with_manager_host},
    verifier::JoinHostVerifier,
};
use crate::{
    SITE_FILE,
    atomic::{FileSystem, StdFileSystem},
    initial::{
        inputs::{ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices},
        request::{InitialApplicationRuntime, derive_site_node_id},
        validation::SiteInputBundleValidator,
    },
    manager_trust::{
        ManagerTrust, SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY, validate_for_private_startup,
    },
    membership::OfflineSite,
    node::folder::{
        CertificateFingerprint, NODE_CERTIFICATE_FILE, NODE_PRIVATE_KEY_FILE, NodeFolderValidator,
        WORKER_MANIFEST_FILE, WorkerNodeCustody,
    },
    private_transport::{
        PinnedPeerVerifier, PrivateNodeRole, PrivatePeerPolicy, PrivateServerConfiguration, pem,
    },
    validation::load_validated_site,
};

#[tokio::test]
async fn successful_join_installs_a_startable_folder_and_records_exactly_one_node() {
    let harness = Harness::start(Options::default()).await;
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
    // The proposed host is not an address of this machine, so the listener binds the route
    // address to the manager and the network must translate.
    assert_eq!(joined.private_bind_address.to_string(), "127.0.0.1:7443");
    assert!(
        joined
            .address_translation_warning(&SystemHostResolver)
            .await
            .expect("translation warning")
            .contains("192.0.2.7:7443")
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
        "https://127.0.0.1:7443/"
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
    assert!(reviews[0].host_differs_from_peer);

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
        allow_observed_loopback: true,
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
    let node_root = harness.node_root();
    let mut observed = proposal("worker", WORKER_HOST);
    observed.host = None;

    let joined = JoinConnection::connect(
        harness.token.clone(),
        Some(ipv4_host(DIALED)),
        &Timing::production().join,
    )
    .await
    .expect("connect at the dialed manager address")
    .join(&observed, &NodeKey::generate().expect("key"), &node_root)
    .await
    .expect("split join");
    let recorded = harness.finish().await.expect("host result");

    assert_eq!(joined.host, ipv4_host(SOURCE));
    assert_eq!(joined.private_bind_address.to_string(), "127.0.0.3:7443");
    assert_eq!(
        joined
            .address_translation_warning(&SystemHostResolver)
            .await,
        None
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
        "https://127.0.0.2:7443/"
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
    assert_eq!(joined.private_bind_address.to_string(), "127.0.0.5:7443");
    assert_eq!(
        joined
            .address_translation_warning(&SystemHostResolver)
            .await,
        None
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
        "https://localhost:7443/"
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
        "https://localhost:7443/"
    );
}

#[tokio::test]
async fn a_proposed_host_name_is_recorded_as_given_and_bound_by_name() {
    let resolver =
        Arc::new(FakeHostResolver::default().with("gpu-box.example.ts.net", [Ipv4Addr::LOCALHOST]));
    let harness = Harness::start(Options {
        resolver: Some(Arc::clone(&resolver)),
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
    assert_eq!(
        joined.private_bind_address.to_string(),
        "gpu-box.example.ts.net:7443"
    );
    assert_eq!(
        joined.address_translation_warning(resolver.as_ref()).await,
        None
    );
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("valid node folder");
    assert_eq!(
        folder.private_bind_address().to_string(),
        "gpu-box.example.ts.net:7443"
    );
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
async fn an_implicit_loopback_observation_is_rejected_with_the_unusable_host_reason() {
    let harness = Harness::start(Options::default()).await;
    let site_before = harness.site_bytes();
    let node_root = harness.node_root();
    let key = NodeKey::generate().expect("key");
    let mut connection = harness.connect().await;
    let mut observed = proposal("worker", WORKER_HOST);
    observed.host = None;

    let error = connection
        .join(&observed, &key, &node_root)
        .await
        .expect_err("observed loopback address");
    let NodeJoinError::UnusableHost(status) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "the manager observed this node at 127.0.0.1, which it cannot record as the node \
         address; provide the address the manager reaches this node at"
    );
    assert!(!node_root.exists());
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.approval.reviews().is_empty());

    connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect("an explicit host joins");
    harness.finish().await.expect("host result");
}

#[tokio::test]
async fn wrong_secrets_are_rejected_without_writes_and_stop_the_host_at_the_limit() {
    let harness = Harness::start(Options {
        limits: JoinHostLimits {
            max_failed_attempts: 2,
            ..test_limits()
        },
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();
    let mut wrong = harness.token.clone();
    wrong.secret = JoinSecret::from_bytes([9; JOIN_SECRET_BYTES]);
    let mut connection = JoinConnection::connect(wrong, None, &Timing::production().join)
        .await
        .expect("the host is authentic even with a wrong secret");
    let node_root = harness.node_root();

    let error = connection
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect_err("wrong secret");
    assert!(
        matches!(&error, NodeJoinError::Join(status) if status.code() == Code::Unauthenticated),
        "{error:?}"
    );
    assert!(!node_root.exists());
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());
    assert!(harness.approval.reviews().is_empty());

    connection
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &node_root,
        )
        .await
        .expect_err("second wrong secret");
    assert!(matches!(
        harness.finish().await,
        Err(JoinHostError::TooManyFailedAttempts)
    ));
    assert_eq!(harness.site_bytes(), site_before);
    assert!(harness.pins().is_empty());
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
    }

    connection
        .join(&proposal("worker", WORKER_HOST), &key, &node_root)
        .await
        .expect("corrected retry with the same token");
    assert_eq!(harness.finish().await.expect("host").display_name, "worker");
    assert_eq!(harness.pins().len(), 1);
}

#[tokio::test]
async fn an_abandoned_offer_leaves_the_site_unchanged_and_frees_the_join() {
    let harness = Harness::start(Options::default()).await;
    let site_before = harness.site_bytes();
    let mut abandoned = harness.raw_client().await;
    let offer = request_offer(&mut abandoned, &harness.token, "worker").await;
    assert!(!offer.offer_id.is_empty());
    assert_eq!(offer.node_host, WORKER_HOST);
    assert_eq!(offer.manager_private_endpoint, "https://127.0.0.1:7443/");
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
        .join(proposal("worker", WORKER_HOST).to_request(
            &harness.token.secret,
            &SiteNodeHost::LOOPBACK,
            NodeKey::generate().expect("key").csr_der().expect("csr"),
        ))
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

    let error = JoinConnection::connect(token, None, &Timing::production().join)
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
        limits: JoinHostLimits {
            idle_timeout: Duration::from_millis(200),
            ..test_limits()
        },
        review_delay: Duration::from_millis(800),
        ..Options::default()
    })
    .await;
    // The held client never commits; the host closes its idle connection and frees the join.
    let mut held = harness.raw_client().await;
    let _offer = request_offer(&mut held, &harness.token, "held").await;

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
    .expect("a review longer than the idle timeout still completes the join");
    assert_eq!(
        harness.finish().await.expect("host").node_id,
        joined.node_id
    );
    assert_eq!(harness.site().nodes.len(), 2);
    drop(held);
}

#[tokio::test]
async fn a_review_is_cancelled_when_its_connection_closes_or_the_host_stops() {
    let harness = Harness::start(Options {
        hold_reviews: true,
        limits: JoinHostLimits {
            max_failed_attempts: 1,
            ..test_limits()
        },
        ..Options::default()
    })
    .await;
    let site_before = harness.site_bytes();

    let mut abandoned = harness.raw_client().await;
    let token = harness.token.clone();
    let pending = tokio::spawn(async move {
        let _ = request_offer_result(&mut abandoned, &token, "worker").await;
    });
    retry_until(|| !harness.approval.reviews().is_empty()).await;
    pending.abort();
    let _ = pending.await;
    harness.approval.wait_cancelled(0).await;

    let mut held = harness.raw_client().await;
    let token = harness.token.clone();
    let pending =
        tokio::spawn(async move { request_offer_result(&mut held, &token, "worker").await });
    retry_until(|| harness.approval.reviews().len() == 2).await;
    let mut wrong = harness.raw_client().await;
    let mut wrong_token = harness.token.clone();
    wrong_token.secret = JoinSecret::from_bytes([9; JOIN_SECRET_BYTES]);
    request_offer_result(&mut wrong, &wrong_token, "other")
        .await
        .expect_err("wrong secret");
    harness.approval.wait_cancelled(1).await;

    assert!(matches!(
        harness.finish().await,
        Err(JoinHostError::TooManyFailedAttempts)
    ));
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
        limits: JoinHostLimits {
            max_failed_attempts: 1,
            ..test_limits()
        },
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
    let offer = request_offer_result(&mut committing, &harness.token, "worker")
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

    let mut wrong = harness.raw_client().await;
    let mut wrong_token = harness.token.clone();
    wrong_token.secret = JoinSecret::from_bytes([9; JOIN_SECRET_BYTES]);
    request_offer_result(&mut wrong, &wrong_token, "other")
        .await
        .expect_err("wrong secret during the commit");
    release.send(()).expect("release the publication");

    let status = commit
        .await
        .expect("commit task")
        .expect_err("failed commit");
    assert_eq!(status.code(), Code::Aborted);
    assert!(matches!(
        harness.finish().await,
        Err(JoinHostError::TooManyFailedAttempts)
    ));
    assert!(harness.pins().is_empty());
}

#[tokio::test]
async fn connections_beyond_the_limit_are_refused_until_one_closes() {
    let harness = Harness::start(Options {
        limits: JoinHostLimits {
            max_connections: 1,
            ..test_limits()
        },
        ..Options::default()
    })
    .await;
    let held = harness.raw_client().await;

    let error = JoinConnection::connect(harness.token.clone(), None, &Timing::production().join)
        .await
        .err()
        .expect("a second connection is refused");
    assert!(matches!(error, NodeJoinError::Connect(_)), "{error:?}");

    drop(held);
    let mut connected = None;
    for _ in 0..200 {
        if let Ok(connection) =
            JoinConnection::connect(harness.token.clone(), None, &Timing::production().join).await
        {
            connected = Some(connection);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    connected
        .expect("a connection is accepted once the first closed")
        .join(
            &proposal("worker", WORKER_HOST),
            &NodeKey::generate().expect("key"),
            &harness.node_root(),
        )
        .await
        .expect("join");
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
        .connect_with(&JoinTiming {
            commit_timeout: Duration::from_millis(200),
            ..Timing::production().join
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
async fn the_host_stops_when_the_token_expires() {
    let harness = Harness::start(Options {
        limits: JoinHostLimits {
            overall_timeout: Duration::ZERO,
            ..test_limits()
        },
        ..Options::default()
    })
    .await;

    assert!(matches!(
        harness.finish().await,
        Err(JoinHostError::Expired)
    ));
    assert!(harness.pins().is_empty());
}

#[tokio::test]
async fn a_rejoin_with_a_new_host_keeps_the_node_and_changes_only_its_addresses() {
    let resolver =
        Arc::new(FakeHostResolver::default().with("gpu-box.example.ts.net", [Ipv4Addr::LOCALHOST]));
    let harness = rejoin_harness(
        &apple_proposal("worker", WORKER_HOST),
        Options {
            resolver: Some(Arc::clone(&resolver)),
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
    assert_eq!(folder.private_port(), 7443);
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
        "gpu-box.example.ts.net:7443"
    );
    assert_eq!(folder.stored_declared_machine(), None);
    assert_eq!(
        folder.manager_endpoint().as_str(),
        "https://localhost:7443/"
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
            private_port: 7443,
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
    let resolver = Arc::new(
        FakeHostResolver::default().with("gpu-box.example.ts.net", [Ipv4Addr::new(192, 0, 2, 7)]),
    );
    let harness = rejoin_harness(
        &proposal("worker", "gpu-box.example.ts.net"),
        Options {
            resolver: Some(Arc::clone(&resolver)),
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
    fs::write(
        harness.fixture.root.join(SITE_FILE),
        dotcore_proto::yaml::to_string(&site).expect("site yaml"),
    )
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
        .rejoin(&rejoin_proposal("192.0.2.8"), &folder)
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
        .rejoin(&rejoin_proposal("192.0.2.8"), &folder)
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
        ("192.0.2.8", "localhost")
    );

    let harness = harness.restart(Options::default()).await;
    let folder = RejoiningNodeFolder::open(&node_root, &harness.token).expect("open folder");
    harness
        .connect_at(Some("localhost"))
        .await
        .rejoin(&rejoin_proposal("192.0.2.8"), &folder)
        .await
        .expect("a repeated rejoin");
    drop(folder);
    harness.finish().await.expect("host result");

    assert_eq!(harness.site_bytes(), site_bytes);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        "https://localhost:7443/"
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
    let error = JoinConnection::connect(token, Some(host("localhost")), &Timing::production().join)
        .await
        .expect("connect through the proxy")
        .rejoin(&rejoin_proposal("192.0.2.8"), &folder)
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
        .rejoin(&rejoin_proposal("192.0.2.8"), &folder)
        .await
        .expect("a repeated rejoin");
    drop(folder);
    harness.finish().await.expect("host result");

    assert_eq!(harness.site_bytes(), site_bytes);
    let folder = NodeFolderValidator::validate_declared(&node_root).expect("rejoined folder");
    assert_eq!(
        folder.manager_endpoint().as_str(),
        "https://localhost:7443/"
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
        .rejoin(&rejoin_proposal("192.0.2.8"), &folder)
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
        "https://localhost:7443/"
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

/// Which refusal variant a join error is.
#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    Join,
    DuplicateName,
    DuplicateHost,
}

/// Changes one pinned value of a token.
type TokenEdit = fn(&mut JoinToken, &CertificateFingerprint);

/// Proposed worker host; 192.0.2.0/24 is reserved for documentation, so it is never an address
/// of the test machine.
const WORKER_HOST: &str = "192.0.2.7";

fn proposal(display_name: &str, proposed_host: &str) -> JoinProposal {
    JoinProposal {
        display_name: display_name.to_owned(),
        host: Some(host(proposed_host)),
        private_port: 7443,
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

fn test_limits() -> JoinHostLimits {
    JoinHostLimits {
        shutdown_grace: Duration::from_millis(100),
        ..JoinHostLimits::default()
    }
}

struct Options {
    limits: JoinHostLimits,
    /// Host the site declares for the manager and the token names; the listener's loopback
    /// address when `None`.
    manager_host: Option<&'static str>,
    /// Resolver both sides look host names up with; the system resolver when `None`.
    resolver: Option<Arc<FakeHostResolver>>,
    allow_observed_loopback: bool,
    /// Binds the join listener on a reserved port, for tests that bind the same number on
    /// another address.
    reserved_listener_port: bool,
    decisions: Vec<JoinDecision>,
    review_delay: Duration,
    hold_reviews: bool,
    after_publish: Option<PublishHook>,
    failing_publish: Option<FailingPublish>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limits: test_limits(),
            manager_host: None,
            resolver: None,
            allow_observed_loopback: false,
            reserved_listener_port: false,
            decisions: Vec::new(),
            review_delay: Duration::ZERO,
            hold_reviews: false,
            after_publish: None,
            failing_publish: None,
        }
    }
}

/// A running join host over an initialized site, with a scripted operator.
struct Harness {
    fixture: InitializedSite,
    token: JoinToken,
    host_address: SocketAddr,
    /// Keeps the join listener's number from other test processes while the harness lives.
    _reserved_port: Option<ReservedPort>,
    approval: Arc<ApprovalScript>,
    site_writer: Arc<RecordingSite>,
    resolver: Arc<dyn HostResolver>,
    host: Mutex<Option<JoinHandle<Result<JoinedNode, JoinHostError>>>>,
    client_directory: tempfile::TempDir,
}

impl Harness {
    async fn start(options: Options) -> Self {
        let fixture = match options.manager_host {
            Some(host) => initialized_site_with_manager_host(host),
            None => initialized_site(),
        };
        let site_writer = Arc::new(RecordingSite {
            inner: OfflineSite::open(&fixture.root, &SiteInputBundleValidator::for_test())
                .expect("open site"),
            after_publish: Mutex::default(),
            failing_publish: Mutex::default(),
            signed_requests: Mutex::default(),
        });
        let client_directory =
            dotcore_domain::test_support::canonical_tempdir().expect("client directory");
        Self::serve(fixture, site_writer, client_directory, options).await
    }

    /// Serves a new host with a fresh token over the same site and joining machine, as a later
    /// node addition on the same stopped manager would. The previous host must have finished.
    async fn restart(self, options: Options) -> Self {
        let Self {
            fixture,
            site_writer,
            client_directory,
            ..
        } = self;
        Self::serve(fixture, site_writer, client_directory, options).await
    }

    async fn serve(
        fixture: InitializedSite,
        site_writer: Arc<RecordingSite>,
        client_directory: tempfile::TempDir,
        options: Options,
    ) -> Self {
        site_writer.set_hooks(options.after_publish, options.failing_publish);
        let identity = site_writer.inner.host_identity().expect("manager identity");
        let reserved_port = options.reserved_listener_port.then(reserve_port);
        let listener = TcpListener::bind((
            Ipv4Addr::LOCALHOST,
            reserved_port.as_ref().map_or(0, ReservedPort::port),
        ))
        .await
        .expect("join listener");
        let host_address = listener.local_addr().expect("listener address");
        let token_host = options
            .manager_host
            .map_or_else(|| host_address.ip().to_string(), str::to_owned);
        let token = identity
            .token(
                format!("https://{token_host}:{}", host_address.port())
                    .parse()
                    .expect("join endpoint"),
            )
            .expect("token");
        let resolver: Arc<dyn HostResolver> = match options.resolver {
            Some(resolver) => resolver,
            None => Arc::new(SystemHostResolver),
        };
        let approval = Arc::new(ApprovalScript {
            delay: options.review_delay,
            hold: options.hold_reviews,
            decisions: Mutex::new(options.decisions.into()),
            reviews: Mutex::default(),
        });
        let host = JoinHost::new(
            JoinHostSettings {
                identity,
                token: token.clone(),
                manager_private_port: 7443,
                allow_observed_loopback: options.allow_observed_loopback,
                limits: options.limits,
            },
            Arc::clone(&site_writer) as Arc<dyn JoinSiteWriter>,
            Arc::new(ScriptedApproval(Arc::clone(&approval))) as Arc<dyn JoinApproval>,
        )
        .with_host_resolver(Arc::clone(&resolver));
        Self {
            fixture,
            token,
            host_address,
            _reserved_port: reserved_port,
            approval,
            site_writer,
            resolver,
            host: Mutex::new(Some(tokio::spawn(host.serve(listener)))),
            client_directory,
        }
    }

    async fn connect(&self) -> JoinConnection {
        self.connect_at(None).await
    }

    /// Connects with `timing` instead of the production join bounds.
    async fn connect_with(&self, timing: &JoinTiming) -> JoinConnection {
        JoinConnection::connect(self.token.clone(), None, timing)
            .await
            .expect("connect to the join host")
            .with_host_resolver(Arc::clone(&self.resolver))
    }

    /// Connects at `manager_host` instead of the token's host when given.
    async fn connect_at(&self, manager_host: Option<&str>) -> JoinConnection {
        JoinConnection::connect(
            self.token.clone(),
            manager_host.map(|value| SiteNodeHost::normalize(value).expect("manager host")),
            &Timing::production().join,
        )
        .await
        .expect("connect to the join host")
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

    async fn finish(&self) -> Result<JoinedNode, JoinHostError> {
        let host = self
            .host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("host result is taken once");
        tokio::time::timeout(Duration::from_secs(60), host)
            .await
            .expect("the host finishes")
            .expect("host task")
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

/// The operator's scripted answers, and the reviews they saw with each review's cancellation.
struct ApprovalScript {
    delay: Duration,
    hold: bool,
    decisions: Mutex<VecDeque<JoinDecision>>,
    reviews: Mutex<Vec<(JoinReview, CancellationToken)>>,
}

impl ApprovalScript {
    fn reviews(&self) -> Vec<JoinReview> {
        self.reviews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(review, _)| review.clone())
            .collect()
    }

    /// Waits until the host cancels the review at `index`.
    async fn wait_cancelled(&self, index: usize) {
        let cancel = self.reviews.lock().unwrap_or_else(PoisonError::into_inner)[index]
            .1
            .clone();
        tokio::time::timeout(Duration::from_secs(30), cancel.cancelled())
            .await
            .expect("the review is cancelled");
    }
}

struct ScriptedApproval(Arc<ApprovalScript>);

impl JoinApproval for ScriptedApproval {
    fn review(
        &self,
        review: JoinReview,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, JoinDecision> {
        let script = Arc::clone(&self.0);
        Box::pin(async move {
            script
                .reviews
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((review, cancel.clone()));
            if script.hold {
                cancel.cancelled().await;
                return JoinDecision::Reject;
            }
            tokio::time::sleep(script.delay).await;
            script
                .decisions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or(JoinDecision::Allow)
        })
    }
}

/// The offline site, recording every signing request and running a hook after each write of a
/// node; the hooks are replaced whenever a new host serves the site.
struct RecordingSite {
    inner: OfflineSite,
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
        peers: [(
            folder.worker_node_id().to_owned(),
            PrivatePeerPolicy {
                role: PrivateNodeRole::Worker,
                certificate_sha256: CertificateFingerprint::parse(pin.trim()).expect("pin value"),
            },
        )]
        .into(),
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
    let error = JoinConnection::connect(token, None, &Timing::production().join)
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
    token: &JoinToken,
    display_name: &str,
) -> JoinResponse {
    request_offer_result(client, token, display_name)
        .await
        .expect("offer")
}

async fn request_offer_result(
    client: &mut NodeJoinServiceClient<Channel>,
    token: &JoinToken,
    display_name: &str,
) -> Result<JoinResponse, tonic::Status> {
    client
        .join(proposal(display_name, WORKER_HOST).to_request(
            &token.secret,
            &SiteNodeHost::LOOPBACK,
            NodeKey::generate().expect("key").csr_der().expect("csr"),
        ))
        .await
        .map(tonic::Response::into_inner)
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
        private_port: 7443,
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
