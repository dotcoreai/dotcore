//! Generated initial site inputs remain acceptable to backend preparation.

use std::{
    collections::BTreeSet,
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use dotcore::composition::{Backend, BackendConfiguration, BackendRole, ServerConfiguration};
use dotcore_catalog::OciArtifactPolicy;
use dotcore_domain::SitePaths;
use dotcore_node_hardware::hardware::{AcceleratorUnavailable, AcceleratorVendorAdapter};
use dotcore_proto::dotcore::observation::v1::{AcceleratorFacts, AcceleratorVendor};
use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};
use dotcore_site::initial::{
    input_store::InitialSiteInputWriter,
    inputs::InitialSiteInputs,
    probe::{InitialSiteProbeReport, InitialSiteProber},
    request::{
        InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentityGenerator,
        InitialSiteIdentityKind, InitialSiteRequest,
    },
    validation::SiteInputBundleValidator,
};
use fs4::fs_std::FileExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

static DOCKER_HOST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
const SYSTEM_INFO_JSON: &str = r#"{"OperatingSystem":"NixOS 26.05 (Yarara)","OSType":"linux","Architecture":"amd64","ServerVersion":"fake","Plugins":{"Log":["json-file","local"]}}"#;

#[tokio::test]
async fn joined_node_folders_are_accepted_by_backend_preparation_for_all_combinations() {
    // This suite does not include the shared support module that relaxes file sync.
    dotcore::testing::relax_sync_for_tests();
    let _docker_host_guard = DOCKER_HOST_LOCK
        .get_or_init(tokio::sync::Mutex::default)
        .lock()
        .await;
    let docker = FakeDockerDaemon::start().await;
    let previous_docker_host = env::var_os("DOCKER_HOST");
    unsafe {
        env::set_var(
            "DOCKER_HOST",
            format!("unix://{}", docker.socket_path.display()),
        )
    };

    use InitialApplicationRuntime as Applications;
    use InitialInferenceRuntime as Inference;
    let combinations = [
        (vec![Inference::Simulated], vec![Applications::Simulated]),
        (vec![Inference::Simulated], vec![Applications::Docker]),
        (vec![Inference::Nvidia], vec![Applications::Simulated]),
        (vec![Inference::Nvidia], vec![Applications::Docker]),
        (
            vec![Inference::Nvidia],
            vec![Applications::Docker, Applications::Simulated],
        ),
        (vec![], vec![]),
    ];
    for (index, (inference, applications)) in combinations.into_iter().enumerate() {
        let request = InitialSiteRequest::new(inference, applications)
            .with_site_id_override(format!("startup-site-{index}"))
            .with_node_id_override(format!("startup-node-{index}"));
        let report = probe_for(&request).await;
        let identities = request
            .resolve_identities(&mut FixedIdentityGenerator::new([1, 2, 3]))
            .expect("generated identities");
        let inputs = InitialSiteInputs::compose(&request, identities, &report)
            .expect("generated initial inputs");
        let fixture = SiteFixture::new();
        let site_root = fixture.root.join("site");
        InitialSiteInputWriter::new(SiteInputBundleValidator::new(
            dotcore::model::engine::declaration_validator(),
            dotcore::model::site_declaration_rules(),
        ))
        .create(&site_root, &inputs)
        .expect("write generated initial inputs");
        SiteInputBundleValidator::new(
            dotcore::model::engine::declaration_validator(),
            dotcore::model::site_declaration_rules(),
        )
        // Whole-catalog invariant: generated initial inputs compose with the shipped catalog.
        .with_platform_catalog_root(workspace_root().join("catalog"))
        .with_oci_artifact_policy(OciArtifactPolicy::Development)
        .validate(&site_root)
        .expect("validate generated initial inputs");

        let configuration = BackendConfiguration {
            site_paths: configuration(&site_root),
            telemetry_directory: None,
            dotcore_sources: Default::default(),
            role: BackendRole::Manager(ServerConfiguration {
                management_bind_address: "127.0.0.1:0".parse().expect("management address"),
                inference_bind_address: "127.0.0.1:0".parse().expect("inference address"),
                private_listener: dotcore::composition::ManagerPrivateListener::Configured(
                    "127.0.0.1:0".parse().expect("private address"),
                ),
                allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
            }),
        };
        let backend_configuration = configuration.clone();
        let backend = Backend::prepare(configuration, std::future::pending())
            .await
            .expect("backend preparation accepts generated inputs");

        let lock_path = PathBuf::from(format!("{}.lock", site_root.display()));
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .expect("backend site input lock");
        assert!(
            !contender
                .try_lock_exclusive()
                .expect("exclusive replacement lock probe"),
            "backend preparation must retain the shared site input lock"
        );
        drop(contender);
        drop(backend);
        if index == 0 {
            fs::remove_dir(site_root.join("trust/worker-pins"))
                .expect("remove empty worker pin registry");
            let failure =
                match Backend::prepare(backend_configuration, std::future::pending()).await {
                    Ok(_) => panic!("private startup accepted missing manager trust"),
                    Err(error) => error,
                };
            assert!(
                format!("{failure:?}").contains("trust directory"),
                "{failure:?}"
            );
        }
    }

    let requests = docker.requests();
    restore_docker_host(previous_docker_host);
    docker.stop().await;
    assert!(
        requests.iter().any(|path| path.contains("/version")),
        "Docker-backed combinations must negotiate the fake Docker API"
    );
    assert!(
        requests.iter().any(|path| path == "/info"),
        "Docker-backed combinations must observe the fake daemon platform"
    );
    let system_info: serde_json::Value =
        serde_json::from_str(SYSTEM_INFO_JSON).expect("valid fake Docker SystemInfo JSON");
    assert_eq!(system_info["OperatingSystem"], "NixOS 26.05 (Yarara)");
    assert_eq!(system_info["OSType"], "linux");
    assert!(matches!(
        system_info["Architecture"].as_str(),
        Some("amd64" | "arm64")
    ));
}

async fn probe_for(request: &InitialSiteRequest) -> InitialSiteProbeReport {
    InitialSiteProber::new(Box::new(FakeDockerProbe), Box::new(FakeNvidiaProbe))
        .probe(request)
        .await
        .expect("selected capability probe")
}

fn configuration(root: &Path) -> SitePaths {
    SitePaths::from_values(
        Some(root.to_str().expect("Site root")),
        // Whole-catalog invariant: the generated site starts against the shipped catalog.
        Some(
            workspace_root()
                .join("catalog")
                .to_str()
                .expect("Catalog root"),
        ),
        Some(root.join("cache").to_str().expect("cache root")),
    )
    .expect("Site paths")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn restore_docker_host(previous: Option<std::ffi::OsString>) {
    unsafe {
        match previous {
            Some(value) => env::set_var("DOCKER_HOST", value),
            None => env::remove_var("DOCKER_HOST"),
        }
    }
}

struct FixedIdentityGenerator {
    values: std::collections::VecDeque<u64>,
}

impl FixedIdentityGenerator {
    fn new(values: impl IntoIterator<Item = u64>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl InitialSiteIdentityGenerator for FixedIdentityGenerator {
    fn generate(&mut self, _kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
        self.values
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("test identity source exhausted"))
    }
}

struct FakeDockerProbe;

impl DockerCapabilityProbe for FakeDockerProbe {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        Box::pin(async {
            Ok(DockerCapabilityReport::new(
                dotcore_domain::ExecutionPlatform::linux_amd64(),
                ["/etc/cdi"],
            ))
        })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        Box::pin(async { Ok(BTreeSet::from(["nvidia.com/gpu=gpu0".to_owned()])) })
    }
}

struct FakeNvidiaProbe;

impl AcceleratorVendorAdapter for FakeNvidiaProbe {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        Ok(vec![AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "Test GPU".to_owned(),
            memory_architecture:
                dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated
                    as i32,
            total_memory_bytes: Some(1),
            driver_or_runtime_version: "570.42.01".to_owned(),
            device_id: String::new(),
            vendor_index: Some(0),
            used_memory_bytes: None,
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        }])
    }
}

struct SiteFixture {
    root: PathBuf,
}

impl SiteFixture {
    fn new() -> Self {
        static SEQUENCE: OnceLock<Mutex<u64>> = OnceLock::new();
        let mut sequence = SEQUENCE
            .get_or_init(|| Mutex::new(0))
            .lock()
            .expect("fixture sequence lock");
        let value = *sequence;
        *sequence += 1;
        let root = canonical_temp_dir().join(format!(
            "dotcore-initial-startup-{}-{value}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("fixture root");
        Self { root }
    }
}

impl Drop for SiteFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct FakeDockerDaemon {
    socket_path: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDockerDaemon {
    async fn start() -> Self {
        // Kept short: the socket path must fit the 104-byte `sockaddr_un` limit on macOS.
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let socket_path = canonical_temp_dir().join(format!(
            "dotcore-fd-{}-{}.sock",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("fake Docker socket");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let (shutdown, mut signal) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut signal => break,
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let request_log = Arc::clone(&request_log);
                        tokio::spawn(async move {
                            let mut bytes = Vec::new();
                            let mut buffer = [0_u8; 1024];
                            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                                let Ok(read) = stream.read(&mut buffer).await else { return };
                                if read == 0 {
                                    return;
                                }
                                bytes.extend_from_slice(&buffer[..read]);
                            }
                            let request = String::from_utf8_lossy(&bytes);
                            let path = request
                                .lines()
                                .next()
                                .and_then(|line| line.split_whitespace().nth(1))
                                .unwrap_or_default()
                                .to_owned();
                            request_log.lock().expect("Docker request log").push(path.clone());
                            let body = if path.contains("/version") {
                                r#"{"ApiVersion":"1.48","Version":"fake"}"#
                            } else if path == "/info" {
                                SYSTEM_INFO_JSON
                            } else {
                                "[]"
                            };
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(), body
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        });
                    }
                }
            }
        });
        Self {
            socket_path,
            requests,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("Docker request log").clone()
    }

    async fn stop(mut self) {
        self.shutdown.take().expect("Docker shutdown").send(()).ok();
        self.task.await.expect("Docker server task");
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Returns the system temporary directory with symlinks resolved, because the product rejects
/// symlinked ancestors and macOS `TMPDIR` lives under the `/var` symlink.
fn canonical_temp_dir() -> std::path::PathBuf {
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
}
