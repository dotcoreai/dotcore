//! Fake Docker image derivations for the tests of crates above the artifact manager.
//!
//! A fixture lays out packages in a fake Nix store, already rooted in its artifact cache so
//! preparation never runs Nix, each with a loader that records its runs and, by default, reports
//! success. A fake Docker daemon holds the images those loaders realize once they ran, and records
//! inspections, so preparation verifies them as it would on a real node.

use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use super::{
    ArtifactManager, DockerImageFacts, DockerImageInspectionFuture, DockerImageInspector,
    NixEvaluation, NixEvaluationFuture, NixEvaluator, NixSources, OciImagePreparer, OciPlatform,
    OciPreparationFuture, OciPreparationRequest, StepOutput,
};

/// Diff ID every fixture image reports and records.
const LAYER: &str = "sha256:fixture-layer";

/// Fake store, loaders, and Docker daemon for Docker-delivered Nix artifacts.
#[derive(Clone)]
pub struct DockerImageFixture {
    root: PathBuf,
    daemon: Arc<FakeDockerDaemon>,
}

impl DockerImageFixture {
    /// Lays the fixture out under `root`, which the caller owns and removes.
    pub fn new(root: &Path) -> Self {
        let nix = root.join("nix-bin");
        std::fs::create_dir_all(&nix).expect("fixture nix directory");
        write_executable(&nix.join("nix"), "#!/bin/sh\nexit 1\n");
        Self {
            root: root.to_path_buf(),
            daemon: Arc::new(FakeDockerDaemon::default()),
        }
    }

    /// Adds a registry Docker image derivation package at `store_path` whose image runs on
    /// linux/`architecture`. Its loader reports the reference derived from the store path, and
    /// the fake daemon holds that image once the loader ran.
    pub fn add_image(&self, store_path: &str, architecture: &str) {
        let launch_reference = dotcore_domain::nix::docker_launch_reference(store_path)
            .expect("fixture Docker image store path");
        let entry = store_path.trim_start_matches("/nix/store/");
        let source_reference = format!("dotcore-nix/fixture:{}", &entry[..32]);
        let result = serde_json::json!({
            "reference": launch_reference,
            "imageId": image_id(entry),
            "path": "present",
            "identityVerified": true,
            "platform": format!("linux/{architecture}"),
        });
        self.add_package(
            store_path,
            &serde_json::json!({
                "reference": source_reference,
                "platform": format!("linux/{architecture}"),
                "kind": "registry",
                "identity": { "diffIds": [LAYER] },
            }),
            &format!("echo '{result}'"),
        );
        let image = DockerImageFacts {
            image_id: Some(image_id(entry)),
            platform: OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: architecture.to_owned(),
                variant: None,
            },
            diff_ids: vec![LAYER.to_owned()],
            labels: BTreeMap::new(),
        };
        self.present_after_load(store_path, &launch_reference, image.clone());
        self.present_after_load(store_path, &source_reference, image);
    }

    /// Adds a Docker image derivation package at `store_path` with its `metadata` and a loader,
    /// named after the store entry, that records its run and then runs the shell `body`.
    pub fn add_package(&self, store_path: &str, metadata: &serde_json::Value, body: &str) {
        let entry = store_path.trim_start_matches("/nix/store/");
        let package = self.package(store_path);
        std::fs::create_dir_all(package.join("bin")).expect("fixture bin");
        std::fs::create_dir_all(package.join("share/dotcore")).expect("fixture share");
        std::fs::write(
            package.join("share/dotcore/image.json"),
            metadata.to_string(),
        )
        .expect("fixture metadata");
        write_executable(
            &self.loader_program(store_path),
            &format!(
                "#!/bin/sh\necho run >> '{}'\n{body}\n",
                self.runs_file(store_path).display()
            ),
        );
        let roots = self.roots();
        std::fs::create_dir_all(&roots).expect("fixture roots");
        std::os::unix::fs::symlink(store_path, roots.join(entry)).expect("fixture root");
    }

    /// Makes the fake daemon hold `image` under `reference`.
    pub fn present(&self, reference: &str, image: DockerImageFacts) {
        self.daemon
            .images
            .lock()
            .expect("fixture images")
            .insert(reference.to_owned(), image);
    }

    /// Makes the fake daemon hold `image` under `reference` once the loader of the package at
    /// `store_path` ran.
    pub fn present_after_load(&self, store_path: &str, reference: &str, image: DockerImageFacts) {
        self.daemon
            .loads
            .lock()
            .expect("fixture loads")
            .insert(reference.to_owned(), (self.runs_file(store_path), image));
    }

    /// Returns the references the fake daemon was asked to inspect, in order.
    pub fn inspected(&self) -> Vec<String> {
        self.daemon
            .inspected
            .lock()
            .expect("fixture inspections")
            .clone()
    }

    /// Returns an artifact manager over the fixture's cache, store, and daemon that builds Nix
    /// artifacts for `system`.
    pub fn manager(&self, system: &'static str) -> ArtifactManager {
        self.manager_with(
            system,
            "unix:///run/dotcore-fixture/docker.sock",
            Duration::from_secs(60),
        )
    }

    /// Returns an artifact manager like `manager`, whose loaders target `docker_endpoint` within
    /// `time_limit`.
    pub fn manager_with(
        &self,
        system: &'static str,
        docker_endpoint: &str,
        time_limit: Duration,
    ) -> ArtifactManager {
        ArtifactManager::new(
            self.root.join("cache"),
            Some(self.daemon.clone() as Arc<dyn OciImagePreparer>),
        )
        .expect("fixture artifact manager")
        .with_nix_sources(NixSources::default().with_roots(self.roots()))
        .with_nix_system(Some(system))
        .with_nix_store(self.root.join("store"), self.nix_directory().join("nix"))
        .with_docker_image_loader(docker_endpoint.to_owned(), time_limit)
    }

    /// Returns the loader program of the package at `store_path`.
    pub fn loader_program(&self, store_path: &str) -> PathBuf {
        let entry = store_path.trim_start_matches("/nix/store/");
        self.package(store_path)
            .join("bin")
            .join(entry.split_at(33).1)
    }

    /// Returns the root folder of the fixture's node.
    pub fn roots(&self) -> PathBuf {
        self.root.join("roots")
    }

    /// Returns the directory holding the fixture's fake `nix`.
    pub fn nix_directory(&self) -> PathBuf {
        self.root.join("nix-bin")
    }

    /// Returns how many times the loader of the package at `store_path` ran.
    pub fn loader_runs(&self, store_path: &str) -> usize {
        std::fs::read_to_string(self.runs_file(store_path))
            .map(|runs| runs.lines().count())
            .unwrap_or(0)
    }

    /// Returns where the package at `store_path` lives in the fixture's fake store.
    pub fn package(&self, store_path: &str) -> PathBuf {
        self.root
            .join("store")
            .join(store_path.trim_start_matches('/'))
    }

    fn runs_file(&self, store_path: &str) -> PathBuf {
        self.root.join(format!(
            "{}.runs",
            store_path.trim_start_matches("/nix/store/")
        ))
    }
}

/// Fake Nix evaluator answering every evaluation with one store path and counting evaluations.
pub struct FixedNixEvaluator {
    store_path: String,
    evaluations: Mutex<usize>,
}

impl FixedNixEvaluator {
    /// Returns a development source named `git+file:///src/dotcore` whose every package
    /// evaluates to `store_path`, and the evaluator behind it.
    pub fn development_source(store_path: &str) -> (NixSources, Arc<Self>) {
        let evaluator = Arc::new(Self {
            store_path: store_path.to_owned(),
            evaluations: Mutex::new(0),
        });
        (
            NixSources {
                sources: dotcore_domain::nix::DotcoreSources {
                    development: Some("git+file:///src/dotcore".to_owned()),
                    packaged: None,
                },
                evaluator: evaluator.clone(),
                roots: None,
            },
            evaluator,
        )
    }

    /// Returns how many evaluations ran.
    pub fn evaluations(&self) -> usize {
        *self.evaluations.lock().expect("fixture evaluations")
    }
}

impl NixEvaluator for FixedNixEvaluator {
    fn evaluate<'a>(
        &'a self,
        _installable: &'a str,
        _cancellation: CancellationToken,
    ) -> NixEvaluationFuture<'a> {
        *self.evaluations.lock().expect("fixture evaluations") += 1;
        Box::pin(std::future::ready(Ok(NixEvaluation {
            store_path: self.store_path.clone(),
        })))
    }
}

/// Fake Docker daemon: OCI preparation succeeds, and inspection answers from the fixture images,
/// including those a loader realizes once it ran, and records each reference asked for.
#[derive(Default)]
struct FakeDockerDaemon {
    images: Mutex<BTreeMap<String, DockerImageFacts>>,

    /// Images a loader realizes, by reference, with the file recording that loader's runs.
    loads: Mutex<BTreeMap<String, (PathBuf, DockerImageFacts)>>,
    inspected: Mutex<Vec<String>>,
}

impl OciImagePreparer for FakeDockerDaemon {
    fn prepare(
        &self,
        _request: OciPreparationRequest,
        _cancellation: CancellationToken,
        _output: StepOutput,
    ) -> OciPreparationFuture {
        Box::pin(async { Ok(()) })
    }

    fn as_docker_image_inspector(&self) -> Option<&dyn DockerImageInspector> {
        Some(self)
    }
}

impl DockerImageInspector for FakeDockerDaemon {
    fn inspect_docker_image(&self, reference: String) -> DockerImageInspectionFuture {
        self.inspected
            .lock()
            .expect("fixture inspections")
            .push(reference.clone());
        let image = self
            .images
            .lock()
            .expect("fixture images")
            .get(&reference)
            .cloned()
            .or_else(|| {
                self.loads
                    .lock()
                    .expect("fixture loads")
                    .get(&reference)
                    .filter(|(runs, _)| runs.exists())
                    .map(|(_, image)| image.clone())
            });
        Box::pin(async move { Ok(image) })
    }
}

fn image_id(entry: &str) -> String {
    use sha2::Digest as _;

    format!("sha256:{:x}", sha2::Sha256::digest(entry.as_bytes()))
}

fn write_executable(path: &Path, content: &str) {
    std::fs::write(path, content).expect("fixture executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("fixture permissions");
}
