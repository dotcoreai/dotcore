//! A real site manager started with `dotc server start` on a disposable one-node site whose
//! application execution target uses the simulated runtime, so `dotc application` contract tests
//! can manage real application deployments without Docker or a GPU.
//!
//! The site carries one synthetic catalog application. A test creates deployments of it through
//! the management API, runs `dotc` against the printed management address, and reads the manager's
//! view back to check every transition. The server is stopped and the site removed when dropped.

use std::{
    io::{BufRead, BufReader},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, CreateApplicationDeploymentRequest,
        ListApplicationDeploymentsRequest,
        application_deployment_service_client::ApplicationDeploymentServiceClient,
    },
    deployment::v1::ExactPlacement,
};

use super::command::isolated_dotc;

/// Display name of the site's only node, as `dotc` shows it.
pub const NODE_NAME: &str = "CLI test node";

/// Execution target that created application deployments are placed on.
pub const APPLICATION_TARGET_ID: &str = "cli-app-target";

/// Display name of the synthetic catalog application.
pub const APPLICATION_NAME: &str = "Round trip fixture";

/// How long the server may take to print its startup protocol.
const STARTUP_DEADLINE: Duration = Duration::from_secs(30);

/// How long an expected deployment state may take to appear before the test fails.
const STATE_DEADLINE: Duration = Duration::from_secs(30);

/// Delay between two reads while waiting for a deployment state.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long the server may take to exit after it is asked to stop.
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

const SITE_ID: &str = "cli-app-site";
const NODE_ID: &str = "cli-app-node";
const APPLICATION_ID: &str = "round-trip-fixture";

/// A running site manager, stopped and removed when dropped.
pub struct SimulatedManager {
    root: PathBuf,
    child: Child,
    address: SocketAddr,
    runtime: tokio::runtime::Runtime,
}

impl SimulatedManager {
    /// Initializes a fresh site and starts its manager on free loopback ports.
    pub fn start() -> Self {
        let root = initialize_site();
        let mut child = dotc_with_certificates()
            .args(["server", "start"])
            .env("DOTCORE_SITE_DIR", root.join("site"))
            .env("DOTCORE_CATALOG_DIR", root.join("master-catalog"))
            .env("DOTCORE_CACHE_DIR", root.join("cache"))
            .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
            .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
            .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(root.join("server.stderr")).expect("server stderr file"))
            .spawn()
            .expect("start dotc server");
        let address = management_address(&mut child, &root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("management client runtime");

        Self {
            root,
            child,
            address,
            runtime,
        }
    }

    /// The management address to pass as `--manager` or `DOTCORE_MANAGEMENT_ADDR`.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Creates a deployment of the synthetic application named `name` and returns the manager's
    /// accepted deployment.
    pub fn create_application(&self, name: &str) -> ApplicationDeployment {
        self.runtime.block_on(async {
            self.client()
                .await
                .create_application_deployment(CreateApplicationDeploymentRequest {
                    application_id: APPLICATION_ID.to_owned(),
                    placement: Some(ExactPlacement {
                        site_node_id: NODE_ID.to_owned(),
                        execution_target_id: APPLICATION_TARGET_ID.to_owned(),
                    }),
                    inference_route_id: None,
                    expected_inference_route_generation: 0,
                    application_deployment_name: Some(name.to_owned()),
                    parameter_values: Vec::new(),
                })
                .await
                .unwrap_or_else(|status| {
                    panic!("{}", self.failure(&format!("create rejected: {status:?}")))
                })
                .into_inner()
                .deployment
                .expect("created application deployment")
        })
    }

    /// Returns the manager's current view of deployment `id`, or `None` once it is gone.
    pub fn application(&self, id: &str) -> Option<ApplicationDeployment> {
        self.runtime.block_on(async {
            self.client()
                .await
                .list_application_deployments(ListApplicationDeploymentsRequest {})
                .await
                .unwrap_or_else(|status| {
                    panic!("{}", self.failure(&format!("list rejected: {status:?}")))
                })
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.application_deployment_id == id)
        })
    }

    /// Reads deployment `id` until `expected` holds for the manager's view, and returns that view.
    pub fn wait_for(
        &self,
        id: &str,
        description: &str,
        expected: impl Fn(Option<&ApplicationDeployment>) -> bool,
    ) -> Option<ApplicationDeployment> {
        let deadline = Instant::now() + STATE_DEADLINE;
        loop {
            let deployment = self.application(id);
            if expected(deployment.as_ref()) {
                return deployment;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{}",
                    self.failure(&format!(
                        "deployment {id} did not reach {description} within {STATE_DEADLINE:?}; last: {deployment:?}"
                    ))
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    async fn client(&self) -> ApplicationDeploymentServiceClient<tonic::transport::Channel> {
        ApplicationDeploymentServiceClient::connect(format!("http://{}", self.address))
            .await
            .expect("manager application API connection")
    }

    /// `message` followed by the server's diagnostics, for a failing assertion.
    fn failure(&self, message: &str) -> String {
        format!("{message}\nserver stderr:\n{}", server_stderr(&self.root))
    }
}

impl Drop for SimulatedManager {
    fn drop(&mut self) {
        // SAFETY: the child is owned by this value and not yet reaped, so its PID is still valid.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + EXIT_DEADLINE;
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Creates the site with `dotc site init`, then replaces its declarations with one node, one
/// simulated application target, and the synthetic catalog application.
fn initialize_site() -> PathBuf {
    let root = super::canonical_temp_dir().join(format!(
        "dotc-simulated-manager-{}-{}",
        std::process::id(),
        SITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let site = root.join("site");
    let initialized = dotc_with_certificates()
        .args(["site", "init", "--site-dir"])
        .arg(&site)
        .args([
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--site-id",
            SITE_ID,
            "--node-id",
            NODE_ID,
            "--host",
            "127.0.0.2",
            "--non-interactive",
        ])
        .output()
        .expect("run dotc site init");
    assert_eq!(
        initialized.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    // `site init` leaves its advisory lock file behind; drop it so the server starts unlocked.
    let _ = std::fs::remove_file(format!("{}.lock", site.display()));

    std::fs::write(
        site.join("site.yaml"),
        format!(
            "siteId: {SITE_ID}\nnodes:\n- nodeId: {NODE_ID}\n  host: 127.0.0.2\n  displayName: {NODE_NAME}\n  siteManager: {{}}\nexecutionTargets:\n- executionTargetId: {APPLICATION_TARGET_ID}\n  siteNodeId: {NODE_ID}\n  displayName: {APPLICATION_TARGET_ID}\n  platform: {{ operatingSystem: linux, architecture: amd64 }}\n  applicationRuntime: {{ simulatedRuntime: {{}} }}\nmodelDeployments: []\nmodelDeploymentIdentityAllocator:\n  namespace: 6c1a9f0e2b7d4c13\n  nextSequence: '1'\ninferenceRoutes: []\napplicationDeployments: []\n"
        ),
    )
    .expect("desired site");
    std::fs::write(site.join("node.yaml"), format!("nodeId: {NODE_ID}\n")).expect("node file");
    for directory in ["models", "engines", "applications"] {
        std::fs::create_dir_all(root.join("master-catalog").join(directory))
            .expect("Catalog directory");
    }
    std::fs::write(
        root.join("master-catalog/applications/round-trip-fixture.yaml"),
        APPLICATION_CATALOG,
    )
    .expect("Catalog application");
    std::fs::create_dir_all(root.join("cache")).expect("cache directory");
    root
}

/// Reads the startup protocol from the server's stdout and returns its management address.
fn management_address(child: &mut Child, root: &Path) -> SocketAddr {
    let stdout = child.stdout.take().expect("server stdout");
    let (lines, received) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + STARTUP_DEADLINE;
    let mut output = Vec::new();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("server status") {
            panic!(
                "server exited before startup with {status}; stdout: {output:?}; stderr:\n{}",
                server_stderr(root)
            );
        }
        let Ok(line) = received.recv_timeout(POLL_INTERVAL) else {
            continue;
        };
        if let Some(address) = line.strip_prefix("DOTCORE_MANAGEMENT_ADDRESS=http://") {
            return address.parse().expect("management address");
        }
        output.push(line);
    }
    panic!(
        "server did not print its management address within {STARTUP_DEADLINE:?}; stdout: {output:?}; stderr:\n{}",
        server_stderr(root)
    );
}

fn server_stderr(root: &Path) -> String {
    std::fs::read_to_string(root.join("server.stderr")).unwrap_or_default()
}

/// Builds a `dotc` command with an empty environment, except the CA bundle the test host may need.
fn dotc_with_certificates() -> Command {
    let mut command = isolated_dotc();
    if let Some(certificates) = std::env::var_os("SSL_CERT_FILE") {
        command.env("SSL_CERT_FILE", certificates);
    }
    command
}

/// Synthetic application: one digest-pinned web service with retained storage and a readiness
/// check, which the simulated runtime realizes without pulling the image.
const APPLICATION_CATALOG: &str = r#"applicationId: "round-trip-fixture"
displayName: "Round trip fixture"
version: "1"
graph:
  services:
    - serviceName: "web"
      artifact:
        oci:
          repository:
            registry: { host: "registry.dotcore.test" }
            name: "round-trip-fixture"
          digest:
            algorithm: "sha256"
            encoded: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        supportedPlatforms:
          - { operatingSystem: "linux", architecture: "amd64" }
          - { operatingSystem: "linux", architecture: "arm64" }
      ports: [{ portName: "http", portNumber: 8080 }]
      storageBindings: [{ bindingName: "data", mountPath: "/data", kind: "STORAGE_KIND_DATA", scope: "STORAGE_SCOPE_DEPLOYMENT" }]
      readiness: { httpGet: { portName: "http", path: "/health" } }
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#;

static SITE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
