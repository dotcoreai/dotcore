//! Records membership changes on the running site: a joined node with its execution targets and
//! the pin of its certificate, or a rejoining node's new addresses, and one existing node's
//! whole replacement target set.
//!
//! Each change is persist-or-reject. Once the site lock confirms the change is valid and a joined
//! node is new, its pin is written, then `site.yaml`; only then is the change accepted in memory
//! and the pin trusted by the private listener and the worker channels, before controllers hear of
//! it. A failure records nothing and removes the pin it wrote, never another node's, so a joining
//! node's commit is never confirmed unless the site holds it. A write whose outcome stays
//! uncertain keeps its pin with the site file that may declare the node, and says so.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use dotcore_proto::dotcore::site::v1::{ExecutionTarget, Site, SiteNode};

use super::{
    join::{JoinSiteError, JoinSiteWriter},
    rescan::{
        ReplacementRole, RescanBase, RescanCommitError, RescanReplacement, RescanSiteWriter,
        record_replacement,
    },
};
use crate::{
    atomic::{StdFileSystem, persist_bytes},
    manager_trust::{ManagerTrust, ManagerTrustError, SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    node::folder::CertificateFingerprint,
    persistence::SiteStorageError,
    private_transport::{PeerPins, PrivateNodeRole, PrivatePeerPolicy},
    repository::{
        MembershipSiteTransaction, ScopedSiteMutationError, SiteRepository, SiteRepositoryError,
    },
    validation::{SiteDeclarationRules, validate_site},
};

/// The running manager's membership writer: it reads and changes the site through the
/// repository, keeps the worker pins on disk, and updates the live pins every private check
/// reads.
pub struct MembershipRecorder {
    site: SiteRepository,
    site_root: PathBuf,
    pins: PeerPins,
}

impl MembershipRecorder {
    /// Records into `site`, whose files live under `site_root`, and trusts each joined node in
    /// `pins`.
    pub fn new(site: SiteRepository, site_root: &Path, pins: PeerPins) -> Self {
        Self {
            site,
            site_root: site_root.to_owned(),
            pins,
        }
    }
}

impl JoinSiteWriter for MembershipRecorder {
    fn declaration_rules(&self) -> SiteDeclarationRules {
        self.site.declaration_rules()
    }

    fn site(&self) -> Result<Site, JoinSiteError> {
        self.site
            .snapshot()
            .map_err(|source| JoinSiteError::Failed(anyhow::Error::new(source)))
    }

    fn sign_node_certificate(
        &self,
        site: &Site,
        node: &SiteNode,
        csr_der: &[u8],
    ) -> Result<Vec<u8>, JoinSiteError> {
        ManagerTrust::load(&self.site_root, site)
            .and_then(|trust| trust.sign_csr(&site.site_id, &node.node_id, csr_der))
            .map_err(|error| match error {
                ManagerTrustError::InvalidCsr(_) => {
                    JoinSiteError::Rejected(anyhow::Error::new(error))
                }
                ManagerTrustError::Issuance(_)
                | ManagerTrustError::Invalid(_)
                | ManagerTrustError::Storage(_) => JoinSiteError::Failed(anyhow::Error::new(error)),
            })
    }

    fn publish_node(
        &self,
        node: &SiteNode,
        targets: &[ExecutionTarget],
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError> {
        let pin = pin_path(&self.site_root, &node.node_id);
        let fingerprint = CertificateFingerprint::for_certificate(certificate_der);
        let mut wrote_pin = false;
        let recorded = self.site.mutate_membership(
            |site| {
                add_node(site, node, targets)?;
                // The node is new, so a pin under its id is a leftover of an attempt that never
                // reached site.yaml, which this one replaces.
                write_pin(&pin, &fingerprint)?;
                wrote_pin = true;
                Ok(())
            },
            |()| {
                self.pins.insert(
                    node.node_id.clone(),
                    PrivatePeerPolicy {
                        role: PrivateNodeRole::Worker,
                        certificate_sha256: fingerprint.clone(),
                    },
                );
            },
        );
        let Err(error) = recorded else {
            return Ok(());
        };
        let error = membership_error(error);
        // After an uncertain write site.yaml may declare the node, so its pin stays with it.
        if wrote_pin && !matches!(error, JoinSiteError::OutcomeUncertain(_)) {
            remove_pin(&pin);
        }
        Err(error)
    }

    fn node_pin(&self, node_id: &str) -> Result<Option<CertificateFingerprint>, JoinSiteError> {
        read_node_pin(&self.site_root, node_id).map_err(JoinSiteError::from)
    }

    fn replace_node_hosts(
        &self,
        node: &SiteNode,
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError> {
        if read_node_pin(&self.site_root, &node.node_id)?
            != Some(CertificateFingerprint::for_certificate(certificate_der))
        {
            return Err(JoinSiteError::from(MembershipChangeError::PinMismatch {
                node_id: node.node_id.clone(),
            }));
        }
        self.site
            .mutate_membership(|site| replace_hosts(site.nodes, node), |()| {})
            .map_err(membership_error)
    }
}

impl RescanSiteWriter for MembershipRecorder {
    fn declaration_rules(&self) -> SiteDeclarationRules {
        self.site.declaration_rules()
    }

    fn site(&self) -> Result<Site, RescanCommitError> {
        self.site
            .snapshot()
            .map_err(|source| RescanCommitError::Failed(anyhow::Error::new(source)))
    }

    fn node_pin(&self, node_id: &str) -> Result<Option<CertificateFingerprint>, RescanCommitError> {
        read_node_pin(&self.site_root, node_id)
            .map_err(|source| RescanCommitError::Failed(anyhow::Error::new(source)))
    }

    fn commit_replacement(
        &self,
        base: &RescanBase,
        replacement: &RescanReplacement,
    ) -> Result<(), RescanCommitError> {
        record_replacement(&self.site, base, replacement, ReplacementRole::Worker)
    }
}

/// Returns a copy of `site` with the node and its targets added, after checking that the result
/// is a valid site, the node is not a site manager, its host is new, and every target belongs
/// to it. Nothing is written.
pub(super) fn site_with_node(
    site: &Site,
    rules: &SiteDeclarationRules,
    node: &SiteNode,
    targets: &[ExecutionTarget],
) -> Result<Site, MembershipChangeError> {
    let mut updated = site.clone();
    add_node(
        &mut MembershipSiteTransaction {
            nodes: &mut updated.nodes,
            execution_targets: &mut updated.execution_targets,
            deployments: &updated.model_deployments,
            applications: &updated.application_deployments,
        },
        node,
        targets,
    )?;
    validate_site(&updated, rules).map_err(MembershipChangeError::InvalidSite)?;
    Ok(updated)
}

/// Returns a copy of `site` in which the declared node with `node`'s identity has `node`'s
/// `host` and `manager_host`, after checking that the node is a declared worker, no other node
/// uses its host, and the result is a valid site. Nothing else changes and nothing is written.
pub(super) fn site_with_node_hosts(
    site: &Site,
    rules: &SiteDeclarationRules,
    node: &SiteNode,
) -> Result<Site, MembershipChangeError> {
    let mut updated = site.clone();
    replace_hosts(&mut updated.nodes, node)?;
    validate_site(&updated, rules).map_err(MembershipChangeError::InvalidSite)?;
    Ok(updated)
}

/// Reads the pin the site trust holds for `node_id`; `None` when there is no pin.
pub(super) fn read_node_pin(
    site_root: &Path,
    node_id: &str,
) -> Result<Option<CertificateFingerprint>, MembershipChangeError> {
    let pin = pin_path(site_root, node_id);
    let value = match fs::read_to_string(&pin) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(MembershipChangeError::PinUnreadable(
                anyhow::Error::new(error).context(format!("read node pin {}", pin.display())),
            ));
        }
    };
    CertificateFingerprint::parse(value.trim())
        .map(Some)
        .map_err(|error| {
            MembershipChangeError::PinUnreadable(
                anyhow::Error::new(error).context(format!("parse node pin {}", pin.display())),
            )
        })
}

/// A membership change the site refuses or could not read its inputs for.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MembershipChangeError {
    #[error("a joining node cannot be a site manager")]
    AdditionalSiteManager,

    #[error("node {node_id:?} already exists in this site")]
    DuplicateNode { node_id: String },

    #[error("node host {host:?} already exists in this site")]
    DuplicateHost { host: String },

    #[error("node {node_id:?} is not declared in this site")]
    UndeclaredNode { node_id: String },

    #[error("the certificate of node {node_id:?} is not the one this site pins")]
    PinMismatch { node_id: String },

    #[error("the node pin could not be read: {0}")]
    PinUnreadable(#[source] anyhow::Error),

    #[error("the node pin could not be written: {0}")]
    PinUnwritable(#[source] anyhow::Error),

    #[error("execution target {target_id:?} does not belong to node {node_id:?}")]
    ForeignTarget { target_id: String, node_id: String },

    #[error("the site with the new node would be invalid: {0}")]
    InvalidSite(#[source] SiteRepositoryError),
}

impl From<MembershipChangeError> for JoinSiteError {
    fn from(error: MembershipChangeError) -> Self {
        match error {
            MembershipChangeError::PinUnreadable(_) | MembershipChangeError::PinUnwritable(_) => {
                Self::Failed(anyhow::Error::new(error))
            }
            MembershipChangeError::AdditionalSiteManager
            | MembershipChangeError::DuplicateNode { .. }
            | MembershipChangeError::DuplicateHost { .. }
            | MembershipChangeError::UndeclaredNode { .. }
            | MembershipChangeError::PinMismatch { .. }
            | MembershipChangeError::ForeignTarget { .. }
            | MembershipChangeError::InvalidSite(_) => Self::Rejected(anyhow::Error::new(error)),
        }
    }
}

/// Adds the node and its targets, after checking that the node is new and not a site manager,
/// its host is new, and every target belongs to it.
fn add_node(
    site: &mut MembershipSiteTransaction<'_>,
    node: &SiteNode,
    targets: &[ExecutionTarget],
) -> Result<(), MembershipChangeError> {
    if node.site_manager.is_some() {
        return Err(MembershipChangeError::AdditionalSiteManager);
    }
    if site
        .nodes
        .iter()
        .any(|existing| existing.node_id == node.node_id)
    {
        return Err(MembershipChangeError::DuplicateNode {
            node_id: node.node_id.clone(),
        });
    }
    if site.nodes.iter().any(|existing| existing.host == node.host) {
        return Err(MembershipChangeError::DuplicateHost {
            host: node.host.clone(),
        });
    }
    if let Some(target) = targets
        .iter()
        .find(|target| target.site_node_id != node.node_id)
    {
        return Err(MembershipChangeError::ForeignTarget {
            target_id: target.execution_target_id.clone(),
            node_id: node.node_id.clone(),
        });
    }
    site.nodes.push(node.clone());
    site.execution_targets.extend(targets.iter().cloned());
    Ok(())
}

/// Gives the declared worker with `node`'s identity `node`'s two addresses, after checking that
/// no other node uses its host.
fn replace_hosts(nodes: &mut [SiteNode], node: &SiteNode) -> Result<(), MembershipChangeError> {
    if nodes
        .iter()
        .any(|other| other.node_id != node.node_id && other.host == node.host)
    {
        return Err(MembershipChangeError::DuplicateHost {
            host: node.host.clone(),
        });
    }
    let declared = nodes
        .iter_mut()
        .find(|declared| declared.node_id == node.node_id)
        .ok_or_else(|| MembershipChangeError::UndeclaredNode {
            node_id: node.node_id.clone(),
        })?;
    if declared.site_manager.is_some() {
        return Err(MembershipChangeError::AdditionalSiteManager);
    }
    declared.host.clone_from(&node.host);
    declared.manager_host.clone_from(&node.manager_host);
    Ok(())
}

/// Projects a refused or failed repository change. An uncertain storage error means `site.yaml`
/// may hold the change; any other means nothing was recorded.
fn membership_error(error: ScopedSiteMutationError<MembershipChangeError>) -> JoinSiteError {
    match error {
        ScopedSiteMutationError::Owner(error) => JoinSiteError::from(error),
        ScopedSiteMutationError::Repository(
            error @ SiteRepositoryError::Storage(SiteStorageError::OutcomeUncertain(_)),
        ) => JoinSiteError::OutcomeUncertain(anyhow::Error::new(error)),
        ScopedSiteMutationError::Repository(
            error @ (SiteRepositoryError::InvalidSite(_)
            | SiteRepositoryError::NodeViewLimitExceeded { .. }),
        ) => JoinSiteError::Rejected(anyhow::Error::new(error)),
        ScopedSiteMutationError::Repository(error) => {
            JoinSiteError::Failed(anyhow::Error::new(error))
        }
    }
}

/// Path of the pin the site trust holds for `node_id`.
fn pin_path(site_root: &Path, node_id: &str) -> PathBuf {
    site_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY)
        .join(format!("{node_id}.sha256"))
}

/// Writes the pin `fingerprint` at `pin`. A failed write leaves no pin there.
fn write_pin(
    pin: &Path,
    fingerprint: &CertificateFingerprint,
) -> Result<(), MembershipChangeError> {
    persist_bytes(
        &StdFileSystem,
        pin,
        format!("{}\n", fingerprint.as_str()).as_bytes(),
    )
    .map_err(|error| {
        remove_pin(pin);
        MembershipChangeError::PinUnwritable(anyhow::Error::new(error).context("write node pin"))
    })
}

/// Removes a pin the site never recorded. A pin that cannot be removed stays as a leftover that
/// the next join of the same node replaces.
fn remove_pin(pin: &Path) {
    if fs::remove_file(pin).is_ok()
        && let Some(pins) = pin.parent()
    {
        let _ = dotcore_domain::fs_sync::sync_directory(pins);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use futures_util::FutureExt as _;

    use super::*;
    use crate::{
        SITE_FILE,
        membership::join::test_fixtures::{
            InitializedSite, initialized_site, worker_node, worker_targets,
        },
        repository::{SiteWriteFault, SiteWriteFaults},
        validation::load_validated_site,
    };

    #[test]
    fn a_published_node_is_written_accepted_and_trusted_with_its_targets_and_pin() {
        let fixture = Fixture::new();
        let node = worker_node("node-worker", "worker", "10.0.0.7");
        let targets = worker_targets("node-worker");
        let root_entries = directory_names(&fixture.site.root);
        let changed = fixture.repository.register_change_listener();

        fixture
            .recorder
            .publish_node(&node, &targets, b"leaf-certificate-der")
            .expect("publish");

        let mut expected = fixture.site.site.clone();
        expected.nodes.push(node.clone());
        expected.execution_targets.extend(targets.iter().cloned());
        assert_eq!(fixture.disk_site(), expected);
        assert_eq!(fixture.repository.snapshot().expect("memory"), expected);
        let fingerprint = CertificateFingerprint::for_certificate(b"leaf-certificate-der");
        assert_eq!(
            fixture.pin_files(),
            [(
                "node-worker.sha256".to_owned(),
                format!("{}\n", fingerprint.as_str())
            )]
        );
        assert_eq!(
            fixture
                .pins
                .get("node-worker")
                .map(|pin| (pin.role, pin.certificate_sha256)),
            Some((PrivateNodeRole::Worker, fingerprint))
        );
        assert_eq!(directory_names(&fixture.site.root), root_entries);
        assert!(
            changed.notified().now_or_never().is_some(),
            "controllers are told"
        );
    }

    #[test]
    fn refused_additions_leave_the_site_pins_and_memory_unchanged() {
        let fixture = Fixture::new();
        let manager = fixture.site.site.nodes[0].clone();
        let mut second_manager = worker_node("node-worker", "worker", "10.0.0.7");
        second_manager.site_manager = manager.site_manager;
        let cases = [
            (
                "duplicate display name",
                worker_node("node-worker", &manager.display_name, "10.0.0.7"),
                worker_targets("node-worker"),
            ),
            (
                "duplicate host",
                worker_node("node-worker", "worker", &manager.host),
                worker_targets("node-worker"),
            ),
            (
                "duplicate node id",
                worker_node(&manager.node_id, "worker", "10.0.0.7"),
                worker_targets(&manager.node_id),
            ),
            (
                "second manager",
                second_manager,
                worker_targets("node-worker"),
            ),
            (
                "foreign target",
                worker_node("node-worker", "worker", "10.0.0.7"),
                worker_targets("node-other"),
            ),
        ];
        for (label, node, targets) in cases {
            let error = fixture
                .recorder
                .publish_node(&node, &targets, b"leaf")
                .expect_err(label);

            assert!(
                matches!(error, JoinSiteError::Rejected(_)),
                "{label}: {error:?}"
            );
            fixture.assert_unchanged(label);
        }
    }

    #[test]
    fn controllers_are_told_of_a_joined_node_only_once_its_pin_is_trusted() {
        let fixture = Fixture::new();
        let trusted_when_told = Arc::new(Mutex::new(Vec::new()));
        let pins = fixture.pins.clone();
        let seen = Arc::clone(&trusted_when_told);
        fixture
            .repository
            .register_change_observer(Arc::new(move |_site: &Site| {
                seen.lock()
                    .expect("observations")
                    .push(pins.get("node-worker").is_some());
            }));

        fixture
            .recorder
            .publish_node(
                &worker_node("node-worker", "worker", "10.0.0.7"),
                &worker_targets("node-worker"),
                b"leaf",
            )
            .expect("publish");

        assert_eq!(*trusted_when_told.lock().expect("observations"), [true]);
    }

    #[test]
    fn a_commit_naming_an_existing_worker_leaves_its_pin_and_trust_intact() {
        let fixture = Fixture::new();
        let worker = worker_node("node-worker", "worker", "10.0.0.7");
        fixture
            .recorder
            .publish_node(&worker, &worker_targets("node-worker"), b"worker leaf")
            .expect("publish the worker");
        let site_bytes = fixture.site_bytes();
        let memory = fixture.repository.snapshot().expect("memory");
        let pins = fixture.pin_files();
        let trusted = CertificateFingerprint::for_certificate(b"worker leaf");

        let error = fixture
            .recorder
            .publish_node(
                &worker_node("node-worker", "impostor", "10.0.0.8"),
                &worker_targets("node-worker"),
                b"impostor leaf",
            )
            .expect_err("the node id is taken");

        assert!(
            matches!(&error, JoinSiteError::Rejected(source)
                if source.to_string() == "node \"node-worker\" already exists in this site"),
            "{error:?}"
        );
        assert_eq!(fixture.site_bytes(), site_bytes);
        assert_eq!(fixture.repository.snapshot().expect("memory"), memory);
        assert_eq!(fixture.pin_files(), pins);
        assert_eq!(
            pins,
            [(
                "node-worker.sha256".to_owned(),
                format!("{}\n", trusted.as_str())
            )]
        );
        assert_eq!(
            fixture
                .pins
                .get("node-worker")
                .map(|pin| pin.certificate_sha256),
            Some(trusted)
        );
    }

    #[test]
    fn an_uncertain_write_whose_write_back_fails_is_reported_uncertain_and_keeps_the_pin() {
        let fixture = Fixture::new();
        fixture.faults.push(SiteWriteFault::Uncertain);
        fixture.faults.push(SiteWriteFault::Storage);
        let node = worker_node("node-worker", "worker", "10.0.0.7");

        let error = fixture
            .recorder
            .publish_node(&node, &worker_targets("node-worker"), b"leaf")
            .expect_err("the write is uncertain");

        assert!(
            matches!(error, JoinSiteError::OutcomeUncertain(_)),
            "{error:?}"
        );
        assert!(
            fixture.disk_site().nodes.contains(&node),
            "the file kept it"
        );
        assert_eq!(
            fixture.repository.snapshot().expect("memory"),
            fixture.site.site
        );
        assert!(fixture.repository.is_dirty());
        assert_eq!(
            fixture.pin_files(),
            [(
                "node-worker.sha256".to_owned(),
                format!(
                    "{}\n",
                    CertificateFingerprint::for_certificate(b"leaf").as_str()
                )
            )]
        );
        assert!(fixture.pins.get("node-worker").is_none(), "not trusted");
    }

    #[test]
    fn a_site_write_that_fails_or_is_uncertain_records_nothing() {
        for fault in [SiteWriteFault::Storage, SiteWriteFault::Uncertain] {
            let fixture = Fixture::new();
            fixture.faults.push(fault);
            let node = worker_node("node-worker", "worker", "10.0.0.7");

            let error = fixture
                .recorder
                .publish_node(&node, &worker_targets("node-worker"), b"leaf")
                .expect_err("the write fails");

            assert!(
                matches!(error, JoinSiteError::Failed(_)),
                "{fault:?}: {error:?}"
            );
            fixture.assert_unchanged(&format!("{fault:?}"));
            assert!(!fixture.repository.is_dirty(), "{fault:?}");
        }
    }

    #[test]
    fn replacing_hosts_changes_only_the_node_addresses_and_writes_nothing_when_equal() {
        let fixture = Fixture::new();
        let node = worker_node("node-worker", "worker", "10.0.0.7");
        fixture
            .recorder
            .publish_node(&node, &worker_targets("node-worker"), b"leaf")
            .expect("publish");
        let pins = fixture.pin_files();
        let published = fixture.repository.snapshot().expect("memory");

        let mut moved = node.clone();
        moved.host = "10.0.0.8".to_owned();
        moved.manager_host = "localhost".to_owned();
        fixture
            .recorder
            .replace_node_hosts(&moved, b"leaf")
            .expect("replace hosts");

        let mut expected = published;
        expected.nodes[1] = moved.clone();
        assert_eq!(fixture.disk_site(), expected);
        assert_eq!(fixture.repository.snapshot().expect("memory"), expected);
        assert_eq!(fixture.pin_files(), pins);

        let writes = fixture.faults.attempts();
        fixture
            .recorder
            .replace_node_hosts(&moved, b"leaf")
            .expect("unchanged hosts");
        assert_eq!(fixture.faults.attempts(), writes);
    }

    #[test]
    fn refused_host_replacements_leave_the_site_and_pins_unchanged() {
        let fixture = Fixture::new();
        let node = worker_node("node-worker", "worker", "10.0.0.7");
        fixture
            .recorder
            .publish_node(&node, &worker_targets("node-worker"), b"leaf")
            .expect("publish");
        let manager = fixture.site.site.nodes[0].clone();
        let site_bytes = fixture.site_bytes();
        let memory = fixture.repository.snapshot().expect("memory");
        let pins = fixture.pin_files();

        let mut manager_moved = manager.clone();
        manager_moved.host = "10.0.0.9".to_owned();
        let cases = [
            (
                "undeclared node",
                worker_node("node-other", "other", "10.0.0.8"),
                &b"leaf"[..],
            ),
            ("manager node", manager_moved, b"leaf"),
            ("pin mismatch", node.clone(), b"other leaf"),
            (
                "duplicate host",
                worker_node("node-worker", "worker", &manager.host),
                b"leaf",
            ),
        ];
        for (label, node, certificate) in cases {
            let error = fixture
                .recorder
                .replace_node_hosts(&node, certificate)
                .expect_err(label);

            assert!(
                matches!(error, JoinSiteError::Rejected(_)),
                "{label}: {error:?}"
            );
            assert_eq!(fixture.site_bytes(), site_bytes, "{label}");
            assert_eq!(
                fixture.repository.snapshot().expect("memory"),
                memory,
                "{label}"
            );
            assert_eq!(fixture.pin_files(), pins, "{label}");
        }
    }

    #[test]
    fn a_leftover_pin_of_an_unrecorded_node_is_replaced() {
        let fixture = Fixture::new();
        fs::write(
            fixture.pin_directory().join("node-worker.sha256"),
            "previous\n",
        )
        .expect("leftover pin");
        let node = worker_node("node-worker", "worker", "10.0.0.7");

        fixture
            .recorder
            .publish_node(&node, &worker_targets("node-worker"), b"leaf")
            .expect("publication replaces the leftover pin");

        assert_eq!(
            fixture.pin_files(),
            [(
                "node-worker.sha256".to_owned(),
                format!(
                    "{}\n",
                    CertificateFingerprint::for_certificate(b"leaf").as_str()
                )
            )]
        );
        assert!(fixture.disk_site().nodes.contains(&node));
    }

    /// A recorder over an initialized site whose writes follow queued faults.
    struct Fixture {
        site: InitializedSite,
        repository: SiteRepository,
        faults: SiteWriteFaults,
        pins: PeerPins,
        recorder: MembershipRecorder,
        site_bytes: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let site = initialized_site();
            let (repository, faults) =
                SiteRepository::load_with_write_faults_for_test(&site.root.join(SITE_FILE))
                    .expect("site repository");
            let pins = PeerPins::default();
            let recorder = MembershipRecorder::new(repository.clone(), &site.root, pins.clone());
            let site_bytes = fs::read(site.root.join(SITE_FILE)).expect("site bytes");
            Self {
                site,
                repository,
                faults,
                pins,
                recorder,
                site_bytes,
            }
        }

        /// Checks that site, pins, live pins and memory are as the fixture started.
        fn assert_unchanged(&self, label: &str) {
            assert_eq!(self.site_bytes(), self.site_bytes, "{label}");
            assert_eq!(
                self.repository.snapshot().expect("memory"),
                self.site.site,
                "{label}"
            );
            assert!(self.pin_files().is_empty(), "{label}");
            assert!(self.pins.get("node-worker").is_none(), "{label}");
        }

        fn disk_site(&self) -> Site {
            load_validated_site(
                &self.site.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules(),
            )
            .expect("site")
        }

        fn site_bytes(&self) -> Vec<u8> {
            fs::read(self.site.root.join(SITE_FILE)).expect("site bytes")
        }

        fn pin_directory(&self) -> PathBuf {
            self.site
                .root
                .join(SITE_TRUST_DIRECTORY)
                .join(WORKER_PIN_DIRECTORY)
        }

        fn pin_files(&self) -> Vec<(String, String)> {
            let mut pins = fs::read_dir(self.pin_directory())
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

    fn directory_names(path: &Path) -> Vec<String> {
        let mut names = fs::read_dir(path)
            .expect("directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}
