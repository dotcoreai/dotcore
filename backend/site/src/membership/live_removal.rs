//! Removes a worker node from the running site, or previews what removing it changes.
//!
//! A removal follows the same rules as on a stopped site: it refuses while deployments are placed
//! on the node, unless the operator forgets them for a node that is not connected. It is
//! persist-or-reject: the site is written first, and only then does the node lose its trust, which
//! closes its open connections. Its pin file is deleted last; a leftover pin of a node the site no
//! longer declares is never trusted.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use dotcore_proto::dotcore::site::v1::Site;
use tracing::info;

use super::removal::{NodeRemovalError, NodeRemovalPlan, plan_node_removal, remove_pin};
use crate::{
    node::control::{CoordinatorConnectionState, ManagerNodeControlCoordinator},
    persistence::SiteStorageError,
    private_transport::PeerPins,
    repository::{ScopedSiteMutationError, SiteRepository, SiteRepositoryError},
    validation::SiteDeclarationRules,
};

/// The running manager's node removal: it changes the site through the repository and withdraws
/// the node's trust from the live pins.
pub struct MachineRemoval {
    site: SiteRepository,
    site_root: PathBuf,
    pins: PeerPins,
    connectivity: Arc<dyn NodeConnectivity>,
}

impl MachineRemoval {
    /// Removes nodes from `site`, whose files live under `site_root`, withdrawing their trust from
    /// `pins`; `connectivity` tells whether a node is connected.
    pub fn new(
        site: SiteRepository,
        site_root: &Path,
        pins: PeerPins,
        connectivity: Arc<dyn NodeConnectivity>,
    ) -> Self {
        Self {
            site,
            site_root: site_root.to_owned(),
            pins,
            connectivity,
        }
    }

    /// Removes the node `node_id`, or only plans it with `validate_only`. With
    /// `forget_deployments`, the deployments placed on the node and the routes they alone serve
    /// are withdrawn with it, which is refused while the node is connected.
    pub fn remove(
        &self,
        node_id: &str,
        forget_deployments: bool,
        validate_only: bool,
    ) -> Result<MachineRemovalOutcome, MachineRemovalError> {
        // Read before the site lock, which the coordinator must never be taken under, so it is
        // advisory: a node that connects right after still keeps the workloads it runs.
        let connected = self.connectivity.is_connected(node_id);
        let rules = self.site.declaration_rules();
        if validate_only {
            let site = self.site.snapshot().map_err(MachineRemovalError::failed)?;
            return Ok(
                match plan_live_removal(&site, &rules, node_id, forget_deployments, connected)? {
                    Some((_, plan)) => MachineRemovalOutcome::WouldRemove(Box::new(plan)),
                    None => MachineRemovalOutcome::AlreadyRemoved,
                },
            );
        }

        let removed = self.site.remove_node(
            |site| match plan_live_removal(site, &rules, node_id, forget_deployments, connected) {
                Ok(Some((updated, plan))) => Ok((updated, plan)),
                Ok(None) => Err(LiveRemovalRefusal::AlreadyRemoved),
                Err(error) => Err(LiveRemovalRefusal::Refused(error)),
            },
            |_| {
                self.pins.remove(node_id);
            },
        );
        let plan = match removed {
            Ok(plan) => plan,
            Err(ScopedSiteMutationError::Owner(LiveRemovalRefusal::AlreadyRemoved)) => {
                return Ok(MachineRemovalOutcome::AlreadyRemoved);
            }
            Err(ScopedSiteMutationError::Owner(LiveRemovalRefusal::Refused(error))) => {
                return Err(error);
            }
            Err(ScopedSiteMutationError::Repository(
                error @ SiteRepositoryError::Storage(SiteStorageError::OutcomeUncertain(_)),
            )) => {
                return Err(MachineRemovalError::OutcomeUncertain(anyhow::Error::new(
                    error,
                )));
            }
            Err(ScopedSiteMutationError::Repository(error)) => {
                return Err(MachineRemovalError::failed(error));
            }
        };
        info!(
            event_kind = "site_node_removed",
            node_id,
            forgotten_deployments = plan.forgotten_deployments.len(),
            deleted_routes = plan.deleted_routes.len(),
            "Site manager removed a node from the site and withdrew its trust"
        );
        remove_pin(&plan.pin_path(&self.site_root));
        Ok(MachineRemovalOutcome::Removed(Box::new(plan)))
    }
}

/// Tells whether a node holds a connection to the site manager now.
pub trait NodeConnectivity: Send + Sync {
    fn is_connected(&self, node_id: &str) -> bool;
}

impl NodeConnectivity for ManagerNodeControlCoordinator {
    fn is_connected(&self, node_id: &str) -> bool {
        self.node_endpoint(node_id).is_some_and(|endpoint| {
            endpoint.connection_state != CoordinatorConnectionState::Disconnected
        })
    }
}

/// What a removal did or would do.
#[derive(Debug)]
pub enum MachineRemovalOutcome {
    /// The site no longer declares the node, and its trust is withdrawn.
    Removed(Box<NodeRemovalPlan>),

    /// What removing the node would change; nothing changed.
    WouldRemove(Box<NodeRemovalPlan>),

    /// The site declares no node with this id, for example after an earlier removal.
    AlreadyRemoved,
}

/// A removal that changed nothing.
#[derive(Debug, thiserror::Error)]
pub enum MachineRemovalError {
    /// The site's rules refuse the removal: the node is the site manager, deployments are placed
    /// on it, or withdrawing them would break routes or the site.
    #[error(transparent)]
    Refused(NodeRemovalError),

    #[error(
        "node {node_name:?} is connected; its deployments can be forgotten only while it is not \
         connected, so remove them first"
    )]
    Connected { node_name: String },

    #[error("node removal failed; the site still declares the node: {0}")]
    Failed(#[source] anyhow::Error),

    #[error(
        "node removal outcome is uncertain; the site still declares the node in memory, retry the \
         removal: {0}"
    )]
    OutcomeUncertain(#[source] anyhow::Error),
}

impl MachineRemovalError {
    fn failed(error: SiteRepositoryError) -> Self {
        Self::Failed(anyhow::Error::new(error))
    }
}

/// Why the removal closure accepted nothing.
#[derive(Debug, thiserror::Error)]
enum LiveRemovalRefusal {
    #[error("the site declares no such node")]
    AlreadyRemoved,

    #[error(transparent)]
    Refused(MachineRemovalError),
}

/// Plans removing `node_id` from `site`: `None` when the site declares no such node, else the
/// site without it and what changes.
fn plan_live_removal(
    site: &Site,
    rules: &SiteDeclarationRules,
    node_id: &str,
    forget_deployments: bool,
    connected: bool,
) -> Result<Option<(Site, NodeRemovalPlan)>, MachineRemovalError> {
    if !site.nodes.iter().any(|node| node.node_id == node_id) {
        return Ok(None);
    }
    let plan = plan_node_removal(site, rules, node_id, forget_deployments)
        .map_err(MachineRemovalError::Refused)?;
    if connected && !plan.forgotten_deployments.is_empty() {
        return Err(MachineRemovalError::Connected {
            node_name: plan.node.display_name.clone(),
        });
    }
    Ok(Some((plan.updated_site().clone(), plan)))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        sync::atomic::{AtomicBool, Ordering},
    };

    use futures_util::FutureExt as _;

    use super::*;
    use crate::{
        SITE_FILE,
        membership::{
            DeploymentKind,
            join::JoinSiteWriter as _,
            recorder::MembershipRecorder,
            removal::tests::{WORKER, WorkerFixture, installed_worker, pin_dir},
        },
        node::folder::CertificateFingerprint,
        private_transport::{PrivateNodeRole, PrivatePeerPolicy},
        repository::{SiteWriteFault, SiteWriteFaults},
        validation::{load_validated_site, validate_site},
    };

    #[test]
    fn removing_a_node_writes_the_site_then_withdraws_its_trust_and_pin() {
        let fixture = Fixture::new(installed_worker());
        let changed = fixture.repository.register_change_listener();

        let outcome = fixture
            .removal
            .remove(WORKER, false, false)
            .expect("remove");

        let MachineRemovalOutcome::Removed(plan) = outcome else {
            panic!("expected a removal, got {outcome:?}");
        };
        assert_eq!(plan.node.node_id, WORKER);
        assert!(plan.forgotten_deployments.is_empty());
        let without = fixture.worker.base.site.clone();
        assert_eq!(fixture.disk_site(), without);
        assert_eq!(fixture.repository.snapshot().expect("memory"), without);
        assert!(fixture.pins.get(WORKER).is_none(), "trust withdrawn");
        assert!(!fixture.pin_file().exists(), "pin deleted");
        assert!(
            changed.notified().now_or_never().is_some(),
            "controllers told"
        );

        // A placement on the removed node is refused from now on.
        let mut dangling = placed_worker().site_with_worker;
        dangling.nodes.retain(|node| node.node_id != WORKER);
        dangling
            .execution_targets
            .retain(|target| target.site_node_id != WORKER);
        assert!(
            validate_site(&dangling, &crate::test_support::site_declaration_rules()).is_err(),
            "a placement on an undeclared node is invalid"
        );

        // The removed node can't join again with its folder.
        let recorder = MembershipRecorder::new(
            fixture.repository.clone(),
            &fixture.worker.root,
            fixture.pins.clone(),
        );
        let node = fixture
            .worker
            .site_with_worker
            .nodes
            .iter()
            .find(|node| node.node_id == WORKER)
            .expect("worker")
            .clone();
        assert!(
            recorder.replace_node_hosts(&node, b"leaf").is_err(),
            "rejoin after removal"
        );

        // A second removal of the same id finds nothing to do.
        assert!(matches!(
            fixture.removal.remove(WORKER, false, false),
            Ok(MachineRemovalOutcome::AlreadyRemoved)
        ));
    }

    #[test]
    fn a_preview_changes_nothing_and_reports_the_plan() {
        let fixture = Fixture::new(placed_worker());

        let outcome = fixture.removal.remove(WORKER, true, true).expect("preview");

        let MachineRemovalOutcome::WouldRemove(plan) = outcome else {
            panic!("expected a preview, got {outcome:?}");
        };
        assert_eq!(
            forgotten(&plan),
            [
                (DeploymentKind::Model, "worker-model".to_owned()),
                (DeploymentKind::Model, "torn-down".to_owned()),
                (DeploymentKind::Application, "worker-app".to_owned()),
            ]
        );
        fixture.assert_unchanged("preview");
    }

    #[test]
    fn placed_deployments_refuse_the_removal_even_while_tearing_down() {
        let fixture = Fixture::new(placed_worker());

        let error = fixture
            .removal
            .remove(WORKER, false, false)
            .expect_err("placed deployments");

        let MachineRemovalError::Refused(NodeRemovalError::PlacedDeployments {
            deployments, ..
        }) = error
        else {
            panic!("expected placed deployments, got {error:?}");
        };
        assert_eq!(
            deployments
                .iter()
                .map(|deployment| (deployment.id.as_str(), deployment.state))
                .collect::<Vec<_>>(),
            [
                ("worker-model", "present"),
                ("torn-down", "teardown"),
                ("worker-app", "present"),
            ]
        );
        fixture.assert_unchanged("refused");
    }

    #[test]
    fn deployments_are_forgotten_only_while_the_node_is_not_connected() {
        let fixture = Fixture::new(placed_worker());
        fixture.connected.store(true, Ordering::SeqCst);

        let error = fixture
            .removal
            .remove(WORKER, true, false)
            .expect_err("connected");
        assert!(
            matches!(error, MachineRemovalError::Connected { ref node_name } if node_name == "worker"),
            "{error:?}"
        );
        fixture.assert_unchanged("connected");

        fixture.connected.store(false, Ordering::SeqCst);
        let MachineRemovalOutcome::Removed(plan) = fixture
            .removal
            .remove(WORKER, true, false)
            .expect("forget while disconnected")
        else {
            panic!("expected a removal");
        };
        assert_eq!(plan.forgotten_deployments.len(), 3);
        let memory = fixture.repository.snapshot().expect("memory");
        assert_eq!(
            memory
                .model_deployments
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            ["upstream"],
            "only the unplaced deployment stays"
        );
        assert!(memory.application_deployments.is_empty());
        assert_eq!(fixture.disk_site(), memory);
    }

    #[test]
    fn the_manager_node_cannot_be_removed() {
        let fixture = Fixture::new(installed_worker());
        let manager = fixture.worker.base.site.nodes[0].node_id.clone();

        let error = fixture
            .removal
            .remove(&manager, true, false)
            .expect_err("manager");

        assert!(
            matches!(
                error,
                MachineRemovalError::Refused(NodeRemovalError::ManagerNode { .. })
            ),
            "{error:?}"
        );
        fixture.assert_unchanged("manager");
    }

    #[test]
    fn a_site_write_that_fails_keeps_the_node_its_trust_and_its_pin() {
        for (faults, uncertain) in [
            (vec![SiteWriteFault::Storage], false),
            // The previous site is written back, so the change is plainly rejected.
            (vec![SiteWriteFault::Uncertain], false),
            (
                vec![SiteWriteFault::Uncertain, SiteWriteFault::Storage],
                true,
            ),
        ] {
            let fixture = Fixture::new(installed_worker());
            for fault in &faults {
                fixture.faults.push(*fault);
            }

            let error = fixture
                .removal
                .remove(WORKER, false, false)
                .expect_err("the write fails");

            assert_eq!(
                matches!(error, MachineRemovalError::OutcomeUncertain(_)),
                uncertain,
                "{faults:?}: {error:?}"
            );
            assert!(
                matches!(
                    error,
                    MachineRemovalError::Failed(_) | MachineRemovalError::OutcomeUncertain(_)
                ),
                "{faults:?}: {error:?}"
            );
            assert_eq!(
                fixture.repository.snapshot().expect("memory"),
                fixture.worker.site_with_worker,
                "{faults:?}"
            );
            assert!(fixture.pins.get(WORKER).is_some(), "{faults:?}: trusted");
            assert!(fixture.pin_file().exists(), "{faults:?}: pin kept");
        }
    }

    /// A live removal over a site with one installed worker, whose writes follow queued faults.
    struct Fixture {
        worker: WorkerFixture,
        repository: SiteRepository,
        faults: SiteWriteFaults,
        pins: PeerPins,
        connected: Arc<AtomicBool>,
        removal: MachineRemoval,
        site_bytes: Vec<u8>,
    }

    impl Fixture {
        fn new(worker: WorkerFixture) -> Self {
            let (repository, faults) =
                SiteRepository::load_with_write_faults_for_test(&worker.root.join(SITE_FILE))
                    .expect("site repository");
            let pins = PeerPins::new(BTreeMap::from([(
                WORKER.to_owned(),
                PrivatePeerPolicy {
                    role: PrivateNodeRole::Worker,
                    certificate_sha256: CertificateFingerprint::for_certificate(b"leaf"),
                },
            )]));
            let connected = Arc::new(AtomicBool::new(false));
            let removal = MachineRemoval::new(
                repository.clone(),
                &worker.root,
                pins.clone(),
                Arc::new(SwitchedConnectivity(Arc::clone(&connected))),
            );
            let site_bytes = fs::read(worker.root.join(SITE_FILE)).expect("site bytes");
            Self {
                worker,
                repository,
                faults,
                pins,
                connected,
                removal,
                site_bytes,
            }
        }

        /// Checks that the site, memory, live pin and pin file are as the fixture started.
        fn assert_unchanged(&self, label: &str) {
            assert_eq!(
                fs::read(self.worker.root.join(SITE_FILE)).expect("site bytes"),
                self.site_bytes,
                "{label}"
            );
            assert_eq!(
                self.repository.snapshot().expect("memory"),
                self.worker.site_with_worker,
                "{label}"
            );
            assert!(self.pins.get(WORKER).is_some(), "{label}");
            assert!(self.pin_file().exists(), "{label}");
        }

        fn disk_site(&self) -> Site {
            load_validated_site(
                &self.worker.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules(),
            )
            .expect("site")
        }

        fn pin_file(&self) -> PathBuf {
            pin_dir(&self.worker.root).join(format!("{WORKER}.sha256"))
        }
    }

    /// A site with one installed worker that runs deployments, kept owner-only as the repository
    /// requires of a site that holds credentials.
    fn placed_worker() -> WorkerFixture {
        let mut worker = installed_worker();
        worker.place_on_worker();
        fs::set_permissions(
            worker.root.join(SITE_FILE),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("owner-only site");
        worker
    }

    /// Connectivity a test switches.
    struct SwitchedConnectivity(Arc<AtomicBool>);

    impl NodeConnectivity for SwitchedConnectivity {
        fn is_connected(&self, _node_id: &str) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn forgotten(plan: &NodeRemovalPlan) -> Vec<(DeploymentKind, String)> {
        plan.forgotten_deployments
            .iter()
            .map(|deployment| (deployment.kind, deployment.id.clone()))
            .collect()
    }
}
