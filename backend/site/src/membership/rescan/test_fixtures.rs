//! Shared fixtures for the re-scan tests: an in-memory site that records what a replacement commit
//! wrote, and the worker site, targets and deployments the handshake and the node-side scan build on.
use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use dotcore_proto::dotcore::{
    catalog::v1::OciPlatform,
    deployment::v1::ExactPlacement,
    model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
    site::v1::{
        ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget, Site,
        application_runtime_binding, execution_target,
    },
};
use tokio::sync::Notify;

use crate::{
    membership::join::test_fixtures::{
        InitializedSite, initialized_site, worker_node, worker_targets,
    },
    membership::rescan::{
        RescanBase, RescanCommitError, RescanReplacement, RescanSiteWriter,
        commit::{ReplacementRole, replace_targets},
    },
    node::folder::CertificateFingerprint,
    repository::MembershipSiteTransaction,
    validation::SiteDeclarationRules,
};

/// The worker node both test files re-scan.
pub(crate) const WORKER: &str = "node-worker";

/// What the next replacement write does to the in-memory site.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum WriteScript {
    /// The replacement is recorded.
    #[default]
    Record,

    /// The write fails; the site keeps what it held.
    Failed,

    /// The write ended without a known outcome; `landed` says whether the site holds the
    /// replacement anyway.
    Uncertain { landed: bool },
}

/// The site a re-scan reads and records into, held in memory so a test can move it between the
/// approval and the commit, and read what the commit left behind.
pub(crate) struct RecordingSite {
    site: Mutex<Site>,
    /// Certificate pin the site trust holds for the worker.
    pin: Mutex<Option<CertificateFingerprint>>,
    /// Whether reading the pin fails, as an unreadable site trust does.
    pin_unreadable: Mutex<bool>,
    /// One entry per recorded replacement: the target ids it installed for the node.
    recorded: Mutex<Vec<Vec<String>>>,
    write: Mutex<WriteScript>,
    /// Holds the next replacement write before it takes the site lock, when a test set one.
    gate: Mutex<Option<HeldWrite>>,
}

impl RecordingSite {
    pub(crate) fn new(site: Site, pin: CertificateFingerprint) -> Self {
        Self {
            site: Mutex::new(site),
            pin: Mutex::new(Some(pin)),
            pin_unreadable: Mutex::new(false),
            recorded: Mutex::new(Vec::new()),
            write: Mutex::new(WriteScript::Record),
            gate: Mutex::new(None),
        }
    }

    /// Replaces what this site declares, keeping every other node's declaration.
    pub(crate) fn replace_site(&self, site: Site) {
        *self.site.lock().expect("site lock") = site;
    }

    /// Sets what the next replacement write does.
    pub(crate) fn set_write(&self, script: WriteScript) {
        *self.write.lock().expect("write lock") = script;
    }

    /// Holds the next replacement write before it takes the site lock until the returned gate
    /// releases it, so a test can act while the commit is under way.
    pub(crate) fn hold_next_write(&self) -> WriteGate {
        let reached = Arc::new(Notify::new());
        let (release, released) = mpsc::channel();
        *self.gate.lock().expect("gate lock") = Some(HeldWrite {
            reached: Arc::clone(&reached),
            released,
        });
        WriteGate { reached, release }
    }

    /// Withholds the worker's certificate pin, as removing the node from the site's trust does.
    pub(crate) fn withdraw_pin(&self) {
        *self.pin.lock().expect("pin lock") = None;
    }

    /// Makes every later read of the worker's pin fail, as an unreadable site trust does.
    pub(crate) fn break_pin_reads(&self) {
        *self.pin_unreadable.lock().expect("pin lock") = true;
    }

    /// Returns the target ids recorded per replacement, in the order they were written.
    pub(crate) fn recorded(&self) -> Vec<Vec<String>> {
        self.recorded.lock().expect("recorded lock").clone()
    }

    /// Returns the ids of the targets this site declares for `node_id`.
    pub(crate) fn target_ids(&self, node_id: &str) -> Vec<String> {
        self.site
            .lock()
            .expect("site lock")
            .execution_targets
            .iter()
            .filter(|target| target.site_node_id == node_id)
            .map(|target| target.execution_target_id.clone())
            .collect()
    }

    fn write_script(&self) -> WriteScript {
        *self.write.lock().expect("write lock")
    }
}

impl RescanSiteWriter for RecordingSite {
    fn declaration_rules(&self) -> SiteDeclarationRules {
        crate::test_support::site_declaration_rules()
    }

    fn site(&self) -> Result<Site, RescanCommitError> {
        Ok(self.site.lock().expect("site lock").clone())
    }

    fn node_pin(
        &self,
        _node_id: &str,
    ) -> Result<Option<CertificateFingerprint>, RescanCommitError> {
        if *self.pin_unreadable.lock().expect("pin lock") {
            return Err(RescanCommitError::Failed(anyhow::anyhow!(
                "the site trust could not be read"
            )));
        }
        Ok(self.pin.lock().expect("pin lock").clone())
    }

    fn commit_replacement(
        &self,
        base: &RescanBase,
        replacement: &RescanReplacement,
    ) -> Result<(), RescanCommitError> {
        if let Some(gate) = self.gate.lock().expect("gate lock").take() {
            gate.hold();
        }
        let script = self.write_script();
        let mut site = self.site.lock().expect("site lock");
        // The same check the live site runs under its lock, on a copy the write then keeps or drops.
        let mut candidate = site.clone();
        replace_targets(
            &mut MembershipSiteTransaction {
                nodes: &mut candidate.nodes,
                execution_targets: &mut candidate.execution_targets,
                deployments: &candidate.model_deployments,
                applications: &candidate.application_deployments,
            },
            base,
            replacement,
            ReplacementRole::Worker,
        )
        .map_err(|error| RescanCommitError::Rejected(anyhow::Error::new(error)))?;
        let failure = match script {
            WriteScript::Failed => Some(RescanCommitError::Failed(anyhow::anyhow!(
                "the site file could not be written"
            ))),
            WriteScript::Uncertain { landed: false } => Some(RescanCommitError::OutcomeUncertain(
                anyhow::anyhow!("the write ended without a known outcome"),
            )),
            WriteScript::Record | WriteScript::Uncertain { landed: true } => None,
        };
        if failure.is_none() {
            *site = candidate;
        }
        drop(site);
        if let Some(failure) = failure {
            return Err(failure);
        }
        self.recorded.lock().expect("recorded lock").push(
            replacement
                .targets()
                .iter()
                .map(|target| target.execution_target_id.clone())
                .collect(),
        );
        Ok(())
    }
}

/// The test's side of a held replacement write: waits for the write to reach the gate, then lets
/// it go on.
pub(crate) struct WriteGate {
    reached: Arc<Notify>,
    release: mpsc::Sender<()>,
}

impl WriteGate {
    /// Waits until the held write reached the gate.
    pub(crate) async fn reached(&self) {
        tokio::time::timeout(GATE_DEADLINE, self.reached.notified())
            .await
            .expect("the replacement write never reached its gate");
    }

    /// Lets the held write go on.
    pub(crate) fn release(&self) {
        self.release.send(()).expect("the held write is waiting");
    }
}

/// The write's side of a held replacement write.
struct HeldWrite {
    reached: Arc<Notify>,
    released: mpsc::Receiver<()>,
}

impl HeldWrite {
    /// Reports the write reached the gate and blocks until the test releases it.
    fn hold(self) {
        self.reached.notify_one();
        self.released
            .recv_timeout(GATE_DEADLINE)
            .expect("the test never released the held write");
    }
}

/// How long either side of a held write waits for the other before the test fails.
const GATE_DEADLINE: Duration = Duration::from_secs(30);

/// Returns the initialized site plus its manager, a declared worker and that worker's two targets.
/// The kept `InitializedSite` holds the fixture's temporary directory.
pub(crate) fn worker_site() -> (InitializedSite, Site) {
    let fixture = initialized_site();
    let mut site = fixture.site.clone();
    site.nodes.push(worker_node(WORKER, "Worker", "10.0.0.7"));
    site.execution_targets.extend(worker_targets(WORKER));
    (fixture, site)
}

/// Returns the worker's committed targets, in site order.
pub(crate) fn committed(site: &Site) -> Vec<ExecutionTarget> {
    site.execution_targets
        .iter()
        .filter(|target| target.site_node_id == WORKER)
        .cloned()
        .collect()
}

/// Returns the worker's committed target with `role` as its name suffix, which its runtime binding
/// carries: `model` or `application`.
pub(crate) fn target_by_role(targets: &[ExecutionTarget], role: &str) -> ExecutionTarget {
    targets
        .iter()
        .find(|target| {
            matches!(
                target.runtime_binding,
                Some(execution_target::RuntimeBinding::ModelRuntime(_))
            ) == (role == "model")
        })
        .cloned()
        .expect("a worker target of this role")
}

/// Declares a model deployment placed on `target_id`, so the deployment guard refuses replacing or
/// removing that target.
pub(crate) fn declare_model_deployment(site: &mut Site, id: &str, target_id: &str) {
    site.model_deployments.push(ModelDeploymentDeclaration {
        model_deployment_id: id.to_owned(),
        model_deployment_name: "Worker model".to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: WORKER.to_owned(),
            execution_target_id: target_id.to_owned(),
        }),
        requested_parallel_shape: dotcore_proto::dotcore::catalog::v1::ModelParallelShape::One
            as i32,
        snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
            "fixture-model",
        )),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        ..Default::default()
    });
}

/// A Docker application target declared on `node_id`, as a node with the Docker applications
/// capability declares it.
pub(crate) fn docker_application_target(node_id: &str) -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: format!("{node_id}-docker-target"),
        display_name: "Docker applications".to_owned(),
        site_node_id: node_id.to_owned(),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: Vec::new(),
        runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
            ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                    DockerApplicationRuntimeAdapter {},
                )),
            },
        )),
    }
}

/// The leaf certificate a test treats as the node's, and its fingerprint, for the pin checks. The
/// handshake compares these bytes; it never parses them.
pub(crate) fn node_certificate() -> (Vec<u8>, CertificateFingerprint) {
    let der = vec![0x30, 0x82, 0x01, 0x02, 0x03];
    let fingerprint = CertificateFingerprint::for_certificate(&der);
    (der, fingerprint)
}

/// The address a test connection claims to come from.
pub(crate) fn peer_address() -> std::net::SocketAddr {
    "127.0.0.1:50000".parse().expect("loopback peer address")
}

/// Waits for `condition` by yielding to it, and reports the last state it observed.
pub(crate) async fn retry_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the condition was never reached");
}
