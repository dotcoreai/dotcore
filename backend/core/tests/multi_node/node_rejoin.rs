//! Proves a worker joins the site again from its intact node folder, on the same host or on a
//! new one, and that after both processes restart it is the same node: the manager lists it
//! connected, the site differs only in the node's host, and its model deployment is Ready again
//! with its route serving.
//!
//! A Docker-gated scenario also rejoins a worker hosting a Docker application at a new address
//! and proves the application is relaunched there with its data kept.
//!
//! The first two scenarios use two real processes with generated worker custody and mutual TLS, and the
//! simulated model runtime. The deployment part needs a host that observes an NVIDIA driver, and
//! the changed-host scenario needs the extra loopback addresses of the split-network harness.

use std::{process::Command, time::Duration};

use dotcore_domain::SiteNodeHost;
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, ApplicationLifecycleState, CreateApplicationDeploymentRequest,
        GetApplicationDeploymentRequest, RemoveApplicationDeploymentRequest,
        application_deployment_service_client::ApplicationDeploymentServiceClient,
    },
    site::v1::NodeObservationState,
};
use dotcore_site::{
    manager_trust::{SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    membership::list_site_nodes,
};
use support::{
    ProcessTestControls, SPLIT_WORKER_HOST, TwoProcessObservationHarness, WORKER_DISPLAY_NAME,
    WORKER_DOCKER_APPLICATION_TARGET_ID, WORKER_HOST_NAME, WORKER_LOOPBACK_HOST, WORKER_NODE_ID,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support;

const BUSYBOX_DIGEST: &str = "73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";
const APPLICATION_ID: &str = "rejoin-docker-fixture";
const APPLICATION_DEADLINE: Duration = Duration::from_secs(120);
const APPLICATION_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DATA_MARKER_FILE: &str = "rejoin-marker";
const DATA_MARKER_CONTENT: &str = "kept across the rejoin";

/// Synthetic site catalog application: a static web server with one data volume.
const DOCKER_APPLICATION_CATALOG: &[(&str, &str)] = &[(
    "applications/rejoin-docker-fixture.yaml",
    r#"applicationId: "rejoin-docker-fixture"
displayName: "Rejoin Docker fixture"
version: "1"
graph:
  services:
    - serviceName: "web"
      artifact:
        oci:
          repository:
            name: "busybox"
          digest:
            algorithm: "sha256"
            encoded: "73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662"
        supportedPlatforms:
          - { operatingSystem: "linux", architecture: "amd64" }
          - { operatingSystem: "linux", architecture: "arm64" }
      command: ["sh", "-ec"]
      arguments:
        - "trap 'exit 0' TERM; mkdir -p /www; printf 'rejoin-ready' > /www/index.html; httpd -f -p 8080 -h /www & wait $!"
      ports: [{ portName: "http", portNumber: 8080 }]
      storageBindings: [{ bindingName: "data", mountPath: "/data", kind: "STORAGE_KIND_DATA", scope: "STORAGE_SCOPE_DEPLOYMENT" }]
      readiness: { httpGet: { portName: "http", path: "/" } }
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
)];

const WORKER_ROUTE: &str = "worker-model";
const MANAGER_NODE_ID: &str = "fixture-node";

#[tokio::test]
async fn a_worker_rejoins_at_its_declared_host_name_and_stays_the_same_node() {
    let Some(harness) =
        TwoProcessObservationHarness::start_named_worker(Default::default(), Default::default())
    else {
        return;
    };

    rejoin_and_verify(harness, WORKER_HOST_NAME.parse().expect("worker host name")).await;
}

#[tokio::test]
async fn a_worker_rejoins_at_a_new_address_and_keeps_its_deployment() {
    if !support::rejoin_loopback_addresses_available() {
        return;
    }
    let Some(harness) = TwoProcessObservationHarness::start() else {
        return;
    };

    rejoin_and_verify(
        harness,
        SiteNodeHost::from_ipv4(SPLIT_WORKER_HOST).expect("specific worker host"),
    )
    .await;
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_worker_application_is_relaunched_at_the_new_address_with_its_data() {
    assert!(
        support::rejoin_loopback_addresses_available(),
        "the rejoin addresses are not bound on this host; run `just dev::mac-loopback`"
    );
    ensure_busybox_image();
    let Some(mut harness) = TwoProcessObservationHarness::start_docker_applications(
        ProcessTestControls {
            site_catalog: DOCKER_APPLICATION_CATALOG,
            ..ProcessTestControls::default()
        },
        ProcessTestControls::default(),
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;

    let mut applications = application_client(manager_address).await;
    let deployment_id = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: APPLICATION_ID.to_owned(),
            placement: Some(support::exact_placement(
                WORKER_NODE_ID,
                WORKER_DOCKER_APPLICATION_TARGET_ID,
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts the Docker application")
        .into_inner()
        .deployment
        .expect("created application");
    let storage_name = deployment_id
        .storage_attachments
        .first()
        .expect("application storage attachment")
        .storage_name
        .clone();
    let deployment_id = deployment_id.application_deployment_id;
    let _containers = ContainerGuard(deployment_id.clone());

    // Ready on the old host, with data the application's volume holds.
    let before = wait_for_ready_at(&mut applications, &deployment_id, WORKER_LOOPBACK_HOST).await;
    let endpoint_before = endpoint_of(&before);
    assert_eq!(http_body(&endpoint_before).await, "rejoin-ready");
    let container_before = group_container(&deployment_id);
    let data = harness
        .worker()
        .site_root()
        .join("applications-data")
        .join("deployment")
        .join(&deployment_id)
        .join(&storage_name);
    std::fs::set_permissions(&data, std::os::unix::fs::PermissionsExt::from_mode(0o777))
        .expect("make the application data directory writable");
    std::fs::write(data.join(DATA_MARKER_FILE), DATA_MARKER_CONTENT).expect("data marker");

    harness.manager_mut().shutdown();
    harness.worker_mut().stop();
    let new_host = SiteNodeHost::from_ipv4(SPLIT_WORKER_HOST).expect("specific worker host");
    harness.rejoin_worker_in_place(new_host);
    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;

    // Relaunched on the new address: same deployment and node, new container and endpoint host,
    // same data.
    let mut applications = application_client(manager_address).await;
    let after = wait_for_ready_at(&mut applications, &deployment_id, SPLIT_WORKER_HOST).await;
    assert_eq!(after.application_deployment_id, deployment_id);
    assert_eq!(
        after
            .state
            .as_ref()
            .and_then(|state| state.observed_placement.as_ref()),
        Some(&support::exact_placement(
            WORKER_NODE_ID,
            WORKER_DOCKER_APPLICATION_TARGET_ID
        ))
    );
    let endpoint_after = endpoint_of(&after);
    assert_eq!(http_body(&endpoint_after).await, "rejoin-ready");
    assert_ne!(group_container(&deployment_id), container_before);
    assert_eq!(
        std::fs::read_to_string(data.join(DATA_MARKER_FILE)).expect("data marker after"),
        DATA_MARKER_CONTENT
    );

    // Teardown removes the container.
    applications
        .remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts the application teardown");
    wait_for_no_group_container(&deployment_id).await;
}

/// Places a model on the connected worker when the host can run it, rejoins the stopped worker
/// at `new_host` from its folder, restarts both processes, and checks that everything but the
/// node's host is as before.
async fn rejoin_and_verify(mut harness: TwoProcessObservationHarness, new_host: SiteNodeHost) {
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let deployment_id = if support::nvidia_driver_observed() {
        let id = support::place_model(
            manager_address,
            WORKER_NODE_ID,
            support::WORKER_MODEL_TARGET_ID,
            WORKER_ROUTE,
        )
        .await;
        support::wait_for_ready(manager_address, &id).await;
        Some(id)
    } else {
        None
    };

    let manager_root = harness.manager().site_root().to_owned();
    let pin = manager_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY)
        .join(format!("{WORKER_NODE_ID}.sha256"));
    let site_before = harness.manager_site();
    let listing_before = list_site_nodes(&manager_root, &dotcore::model::site_declaration_rules())
        .expect("node listing before");
    let pin_before = std::fs::read(&pin).expect("worker pin before");
    let folder_before = harness.node_folder();
    let certificate_before = folder_before.worker_certificate_der().to_vec();
    let trust_before = trust_files(harness.worker().site_root());
    let bind_before = folder_before.private_bind_address().clone();
    drop(folder_before);

    harness.manager_mut().shutdown();
    harness.worker_mut().stop();
    let joined = harness.rejoin_worker_in_place(new_host.clone());

    // Only the node's host changes: site, listing, pin, certificate, trust files (keys included) and private port.
    assert_eq!(joined.node_id, WORKER_NODE_ID);
    assert_eq!(joined.display_name, WORKER_DISPLAY_NAME);
    assert_eq!(joined.host, new_host);
    let mut expected_site = site_before;
    expected_site
        .nodes
        .iter_mut()
        .find(|node| node.node_id == WORKER_NODE_ID)
        .expect("declared worker")
        .host = new_host.to_string();
    assert_eq!(harness.manager_site(), expected_site);
    let mut expected_listing = listing_before;
    expected_listing
        .nodes
        .iter_mut()
        .find(|listed| listed.node.node_id == WORKER_NODE_ID)
        .expect("listed worker")
        .node
        .host = new_host.to_string();
    assert_eq!(
        list_site_nodes(&manager_root, &dotcore::model::site_declaration_rules())
            .expect("node listing after"),
        expected_listing
    );
    assert_eq!(std::fs::read(&pin).expect("worker pin after"), pin_before);
    let folder_after = harness.node_folder();
    assert_eq!(folder_after.worker_node_id(), WORKER_NODE_ID);
    assert_eq!(folder_after.worker_certificate_der(), certificate_before);
    assert_eq!(trust_files(harness.worker().site_root()), trust_before);
    assert_eq!(
        folder_after.private_bind_address().to_string(),
        format!("{new_host}:{}", bind_before.port())
    );
    drop(folder_after);

    // Both processes restart: the node connects on its new host and its deployment converges.
    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    assert_eq!(
        support::machine_states(manager_address).await,
        [
            (
                MANAGER_NODE_ID.to_owned(),
                Some(NodeObservationState::Connected)
            ),
            (
                WORKER_NODE_ID.to_owned(),
                Some(NodeObservationState::Connected)
            ),
        ]
    );
    if let Some(deployment_id) = deployment_id {
        support::wait_for_ready(manager_address, &deployment_id).await;
        assert_eq!(
            support::listed_deployments(manager_address).await,
            [deployment_id.as_str()]
        );
        assert_eq!(
            support::listed_routes(manager_address).await,
            [WORKER_ROUTE]
        );
        assert_eq!(
            support::chat_status(&harness, WORKER_ROUTE).await,
            reqwest::StatusCode::OK
        );
    }
}

/// Returns the bytes of every file in the node folder's trust directory, by file name.
fn trust_files(node_root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(node_root.join(SITE_TRUST_DIRECTORY))
        .expect("node trust directory")
        .map(|entry| {
            let entry = entry.expect("trust entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("trust file"),
            )
        })
        .collect()
}

async fn application_client(
    manager_address: std::net::SocketAddr,
) -> ApplicationDeploymentServiceClient<tonic::transport::Channel> {
    ApplicationDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager application API connection")
}

/// Waits until the manager projects the deployment Ready from current worker status with an
/// endpoint on `host`.
async fn wait_for_ready_at(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    host: std::net::Ipv4Addr,
) -> ApplicationDeployment {
    let mut last = None;
    let result = tokio::time::timeout(APPLICATION_DEADLINE, async {
        loop {
            let deployment = applications
                .get_application_deployment(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                })
                .await
                .expect("manager application status")
                .into_inner()
                .deployment
                .expect("listed application");
            let ready = deployment.state.as_ref().is_some_and(|state| {
                state.lifecycle_state == ApplicationLifecycleState::Ready as i32
                    && !state.observation_stale
                    && state.endpoint.as_ref().is_some_and(|endpoint| {
                        endpoint.url.starts_with(&format!("http://{host}:"))
                    })
            });
            if ready {
                return deployment;
            }
            last = deployment.state;
            tokio::time::sleep(APPLICATION_POLL_INTERVAL).await;
        }
    })
    .await;
    result.unwrap_or_else(|_| {
        panic!("application Ready deadline at {host}; last projected state {last:?}")
    })
}

fn endpoint_of(deployment: &ApplicationDeployment) -> String {
    deployment
        .state
        .as_ref()
        .and_then(|state| state.endpoint.as_ref())
        .map(|endpoint| endpoint.url.clone())
        .expect("application endpoint")
}

/// Fetches `/` from the `http://host:port` endpoint and returns the response body.
async fn http_body(endpoint: &str) -> String {
    let address = endpoint.strip_prefix("http://").expect("http endpoint");
    let mut stream = tokio::net::TcpStream::connect(address.trim_end_matches('/'))
        .await
        .expect("connect to the application endpoint");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: application\r\nConnection: close\r\n\r\n")
        .await
        .expect("send the request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read the response");
    String::from_utf8_lossy(&response)
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default()
}

/// Returns the ids of the Docker containers of the deployment's group, running or not.
fn group_containers(deployment_id: &str) -> Vec<String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=com.dotcore.runtime.group={deployment_id}"),
        ])
        .output()
        .expect("run docker ps");
    assert!(output.status.success(), "docker ps failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Returns the only container of the deployment's group.
fn group_container(deployment_id: &str) -> String {
    let mut containers = group_containers(deployment_id);
    assert_eq!(
        containers.len(),
        1,
        "one container for the deployment, found {containers:?}"
    );
    containers.remove(0)
}

async fn wait_for_no_group_container(deployment_id: &str) {
    tokio::time::timeout(APPLICATION_DEADLINE, async {
        while !group_containers(deployment_id).is_empty() {
            tokio::time::sleep(APPLICATION_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("application containers removed deadline");
}

/// Makes sure the pinned busybox image is on this machine's Docker daemon, pulling it from the
/// public registry when absent.
fn ensure_busybox_image() {
    let image = format!("busybox@sha256:{BUSYBOX_DIGEST}");
    let present = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", &image])
        .output()
        .expect("run docker image inspect")
        .status
        .success();
    if !present {
        let pulled = Command::new("docker")
            .args(["pull", &image])
            .status()
            .expect("run docker pull");
        assert!(pulled.success(), "docker pull {image} failed");
    }
}

/// Removes any container left in the deployment's group, including when the test fails.
struct ContainerGuard(String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let containers = group_containers(&self.0);
        if !containers.is_empty() {
            let _ = Command::new("docker")
                .arg("rm")
                .arg("-f")
                .args(containers)
                .status();
        }
    }
}
