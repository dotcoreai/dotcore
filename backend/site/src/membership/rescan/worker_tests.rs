//! Tests of the node's side of a target re-scan: the capability scan of this machine, the install of
//! the committed projection into the node's own folder, and the custody that keeps a running node
//! from having its targets changed under it.

use std::{collections::BTreeSet, fs, path::PathBuf};

use dotcore_domain::{BindAddress, ExecutionPlatform, test_support::canonical_tempdir};
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::{
    node::v1::{DeclaredMachine, NodeObservationConfiguration},
    site::v1::{ExecutionTarget, Site},
};

use super::installation::{InstalledProjection, RescanInstallError, install_projection};
use super::scan::{RescanCapability, scan_replacement, scan_request};
use super::test_fixtures::{
    WORKER, committed, docker_application_target, target_by_role, worker_site,
};
use super::{NodeRescan, NodeRescanError, derive_replacement, rescan_base};
use crate::{
    atomic::StdFileSystem,
    initial::{
        inputs::{NodeTargetChoices, build_execution_targets},
        probe::{InitialSiteProbeReport, test_probe_report_for_platform},
        request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
    },
    membership::join::{rewrite_bootstrap, test_fixtures::InitializedSite},
    node::folder::{
        CertificateFingerprint, NodeFolderBootstrap, NodeFolderManager, WORKER_MANIFEST_FILE,
        WorkerNodeCustody, read_bootstrap,
    },
    node::observation_configuration::NodeObservationConfigurationDigest,
};

/// Simulated hardware the fixture node declares for its simulated targets.
const SIMULATED: &str = "rtx-5090";

/// The whole projection a node folder installs, and the node's identity, as a joined node folder
/// holds them. A re-scan never changes the identity parts.
struct NodeFolder {
    _directory: tempfile::TempDir,
    root: PathBuf,
    bootstrap: NodeFolderBootstrap,
}

impl NodeFolder {
    /// Writes a node manifest for `base`'s site and node at `root`, installing `targets` under
    /// `observation` and declaring the simulated machine the node's targets were built from.
    fn new(
        site_id: &str,
        observation: NodeObservationConfiguration,
        targets: Vec<ExecutionTarget>,
    ) -> Self {
        let directory = canonical_tempdir().expect("node folder fixture");
        let root = directory.path().join("node");
        fs::create_dir_all(&root).expect("node folder directory");
        let declared = DeclaredMachine {
            choices: vec![SIMULATED.to_owned()],
            description: Some(
                MachineDescription::simulated(&[SIMULATED.to_owned()], "amd64")
                    .expect("simulated machine")
                    .to_stored(),
            ),
        };
        let bootstrap = NodeFolderBootstrap::new(
            site_id.to_owned(),
            WORKER,
            CertificateFingerprint::for_certificate(b"worker-leaf-certificate"),
            NodeFolderManager::new(
                "manager-node",
                "https://10.0.0.6:7443",
                CertificateFingerprint::for_certificate(b"manager-leaf-certificate"),
            )
            .expect("manager endpoint"),
            observation,
            targets,
            BindAddress::parse("10.0.0.7:7443").expect("private bind address"),
        )
        .and_then(|bootstrap| bootstrap.with_declared_machine(Some(declared)))
        .expect("node manifest");
        rewrite_bootstrap(&StdFileSystem, &root, &bootstrap).expect("write the node manifest");
        Self {
            _directory: directory,
            root,
            bootstrap,
        }
    }

    fn manifest_bytes(&self) -> Vec<u8> {
        fs::read(self.root.join(WORKER_MANIFEST_FILE)).expect("the node manifest")
    }
}

/// The projection the site committed for `targets` installed under `observation`.
fn projection(
    observation: &NodeObservationConfiguration,
    targets: Vec<ExecutionTarget>,
) -> InstalledProjection {
    InstalledProjection {
        targets,
        observation: observation.clone(),
        observation_configuration_sha256: NodeObservationConfigurationDigest::for_configuration(
            observation,
        )
        .expect("digest of the committed observation configuration")
        .as_str()
        .to_owned(),
    }
}

/// A projection that digests to something other than the digest the site recorded is refused before
/// any byte is written, because it is not the projection the site described.
#[test]
fn installing_a_projection_that_does_not_carry_its_digest_writes_no_bytes() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let folder = NodeFolder::new(&site.site_id, base.observation.clone(), committed(&site));
    let before = folder.manifest_bytes();

    let mut committed_projection = projection(&base.observation, committed(&site));
    committed_projection.observation_configuration_sha256 = "0".repeat(64);
    let error = install_projection(&folder.root, &committed_projection)
        .expect_err("a projection without its own digest is refused");

    assert!(matches!(error, RescanInstallError::DigestMismatch { .. }));
    assert_eq!(folder.manifest_bytes(), before);
    assert_eq!(
        read_bootstrap(&folder.root).expect("the manifest still reads"),
        folder.bootstrap
    );
}

/// A projection the folder already installs is not written again, which is what makes a no-op
/// re-scan cost no bytes at all.
#[test]
fn installing_the_projection_a_folder_already_installs_writes_no_bytes() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let folder = NodeFolder::new(&site.site_id, base.observation.clone(), committed(&site));
    let before = folder.manifest_bytes();

    let installed = install_projection(
        &folder.root,
        &projection(&base.observation, committed(&site)),
    )
    .expect("the installed projection needs no write");

    assert!(!installed);
    assert_eq!(folder.manifest_bytes(), before);
}

/// Installing the committed projection replaces the manifest with the whole target set and the
/// digest the site recorded, and keeps the node's identities, its pinned manager, its bind address
/// and its declared machine.
#[test]
fn installing_a_committed_projection_replaces_the_manifest_and_keeps_the_nodes_identity() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let folder = NodeFolder::new(&site.site_id, base.observation.clone(), committed(&site));
    let before = folder.manifest_bytes();

    let mut targets = committed(&site);
    let model = target_by_role(&targets, "model");
    for target in &mut targets {
        if target.execution_target_id == model.execution_target_id {
            target.display_name = "Renamed model target".to_owned();
        }
    }
    let installed = install_projection(
        &folder.root,
        &projection(&base.observation, targets.clone()),
    )
    .expect("the committed projection installs");

    assert!(installed);
    assert_ne!(folder.manifest_bytes(), before);
    let written = read_bootstrap(&folder.root).expect("the installed manifest reads back");
    assert_eq!(written.realization_targets, targets);
    assert_eq!(
        written.observation_configuration_sha256.as_str(),
        NodeObservationConfigurationDigest::for_configuration(&base.observation)
            .expect("digest of the committed observation configuration")
            .as_str()
    );
    assert_eq!(written.site_id, folder.bootstrap.site_id);
    assert_eq!(written.worker_node_id, folder.bootstrap.worker_node_id);
    assert_eq!(
        written.worker_certificate_sha256,
        folder.bootstrap.worker_certificate_sha256
    );
    assert_eq!(written.manager, folder.bootstrap.manager);
    assert_eq!(
        written.private_bind_address,
        folder.bootstrap.private_bind_address
    );
    assert_eq!(written.declared_machine, folder.bootstrap.declared_machine);
}

/// A projection whose target set does not match the observation configuration it installs is refused
/// before the manifest is replaced, so the node keeps serving under the projection it had.
#[test]
fn installing_a_projection_whose_targets_do_not_match_its_observation_writes_no_bytes() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let folder = NodeFolder::new(&site.site_id, base.observation.clone(), committed(&site));
    let before = folder.manifest_bytes();

    let mut targets = committed(&site);
    // A target with no counterpart in the observation configuration: the manifest would name a
    // target the node was never told how to observe.
    targets.push(docker_application_target(WORKER));
    let error = install_projection(&folder.root, &projection(&base.observation, targets))
        .expect_err("a projection naming a target its observation does not cover is refused");

    assert!(matches!(error, RescanInstallError::Manifest(_)));
    assert_eq!(folder.manifest_bytes(), before);
}

/// A node whose server holds custody of its folder is refused before its folder is read: its targets
/// are never changed while it serves.
#[tokio::test]
async fn a_running_node_is_refused_before_its_folder_is_re_read() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let folder = NodeFolder::new(&site.site_id, base.observation.clone(), committed(&site));
    let custody =
        WorkerNodeCustody::try_acquire(&folder.root).expect("custody of an idle node folder");

    let error = match NodeRescan::open(&folder.root) {
        Ok(_) => panic!("a running node was opened for a re-scan"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        NodeRescanError::NodeRunning { node_root } if node_root == folder.root
    ));
    drop(custody);
}

/// A node that runs simulated workloads re-scans only its simulated capabilities, even on a machine
/// with real ones: simulated hardware stands in for the real hardware of its kind.
#[test]
fn a_simulated_node_re_scans_only_its_simulated_capabilities() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let committed = committed(&site);
    let declared = DeclaredMachine {
        choices: vec![SIMULATED.to_owned()],
        description: None,
    };

    let request = scan_request(&committed, Some(&declared));
    assert_eq!(
        request.inference(),
        &BTreeSet::from([InitialInferenceRuntime::Simulated])
    );
    assert_eq!(
        request.applications(),
        &BTreeSet::from([InitialApplicationRuntime::Simulated])
    );
    assert_eq!(request.simulated_hardware(), vec![SIMULATED.to_owned()]);

    let scan = scan_replacement(
        WORKER,
        &committed,
        Some(&declared),
        &mac_with_docker(),
        "amd64",
    )
    .expect("the node's scan");

    assert!(scan.unavailable.is_empty());
    assert_eq!(
        capabilities(&scan.targets),
        vec![
            RescanCapability::SimulatedModels,
            RescanCapability::SimulatedApplications,
        ]
    );
    assert!(
        scan.targets
            .iter()
            .all(|target| target.site_node_id == WORKER)
    );

    // The rebuilt set is a whole replacement the site accepts for this node.
    let rules = crate::test_support::site_declaration_rules();
    let replacement = derive_replacement(&site, &rules, WORKER, scan.targets.clone(), Some(&base))
        .expect("the scanned replacement is valid");
    assert_eq!(replacement.targets(), &scan.targets);
}

/// A node declared with only a native model target, on a Mac that now runs Docker, gains a Docker
/// application target: the scan looks for every capability the host has, not only the committed
/// ones, and the native target stays as it was.
#[test]
fn a_native_only_node_gains_the_docker_application_target_its_machine_now_offers() {
    let (_fixture, site) = native_only_worker();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let committed = committed(&site);
    assert_eq!(
        capabilities(&committed),
        vec![RescanCapability::AppleModels]
    );

    let request = scan_request(&committed, None);
    assert_eq!(
        request.inference(),
        &BTreeSet::from([
            InitialInferenceRuntime::Nvidia,
            InitialInferenceRuntime::Apple
        ])
    );
    assert_eq!(
        request.applications(),
        &BTreeSet::from([InitialApplicationRuntime::Docker])
    );

    let scan = scan_replacement(WORKER, &committed, None, &mac_with_docker(), "aarch64")
        .expect("the node's scan");
    assert!(scan.unavailable.is_empty());

    let rules = crate::test_support::site_declaration_rules();
    let replacement = derive_replacement(&site, &rules, WORKER, scan.targets, Some(&base))
        .expect("a replacement adding a target is valid");
    let classification = replacement.classification();
    assert_eq!(classification.unchanged, committed);
    assert_eq!(
        capabilities(&classification.added),
        vec![RescanCapability::DockerApplications]
    );
    let added = &classification.added[0];
    assert_eq!(added.site_node_id, WORKER);
    assert_eq!(added.display_name, "Docker applications");
    assert_eq!(
        added.platform,
        Some(ExecutionPlatform::linux_arm64().to_oci_platform())
    );
    assert!(classification.changed.is_empty());
    assert!(classification.removed.is_empty());
    assert!(!replacement.is_noop());
}

/// Once the added target is committed, the same machine re-scans to the same whole projection: the
/// re-scan is a no-op that writes nothing.
#[test]
fn re_scanning_the_same_machine_after_a_target_was_added_is_a_no_op() {
    let (_fixture, site) = docker_gained_worker();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    assert_eq!(
        capabilities(&committed(&site)),
        vec![
            RescanCapability::AppleModels,
            RescanCapability::DockerApplications,
        ]
    );

    let scan = scan_replacement(
        WORKER,
        &committed(&site),
        None,
        &mac_with_docker(),
        "aarch64",
    )
    .expect("the node's second scan");

    assert_eq!(scan.targets, committed(&site));
    let rules = crate::test_support::site_declaration_rules();
    let replacement = derive_replacement(&site, &rules, WORKER, scan.targets, Some(&base))
        .expect("the repeated replacement is valid");
    assert!(replacement.classification().is_unchanged());
    assert!(replacement.is_noop());
}

/// A native model capability the machine no longer offers keeps its committed target, reported as
/// unavailable, beside the capabilities the machine still offers.
#[test]
fn a_native_capability_the_machine_no_longer_offers_is_kept_and_reported_unavailable() {
    let (_fixture, site) = docker_gained_worker();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let apple = committed(&site)
        .into_iter()
        .filter(|target| RescanCapability::of(target) == Some(RescanCapability::AppleModels))
        .collect::<Vec<_>>();

    // Docker still runs, but the Apple GPU is no longer observed.
    let docker_only =
        test_probe_report_for_platform(Some(ExecutionPlatform::linux_arm64()), None, ["/etc/cdi"]);
    let scan = scan_replacement(WORKER, &committed(&site), None, &docker_only, "aarch64")
        .expect("the node's scan");

    assert_eq!(
        scan.unavailable,
        vec![(RescanCapability::AppleModels, apple)]
    );
    let rules = crate::test_support::site_declaration_rules();
    let replacement = derive_replacement(&site, &rules, WORKER, scan.targets, Some(&base))
        .expect("a replacement keeping an unavailable target is valid");
    assert!(replacement.classification().is_unchanged());
    assert!(replacement.is_noop());
}

/// A capability this machine no longer offers keeps its committed targets in the replacement and is
/// reported by the scan, so a hardware outage removes no targets and the replacement stays valid.
#[test]
fn a_capability_the_machine_no_longer_offers_keeps_its_committed_targets() {
    let (_fixture, site) = worker_site();
    let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
    let app_target = target_by_role(&committed(&site), "application");
    let mut committed = committed(&site);
    for target in &mut committed {
        if target.execution_target_id == app_target.execution_target_id {
            *target = docker_application_target(WORKER);
        }
    }
    let declared = DeclaredMachine {
        choices: vec![SIMULATED.to_owned()],
        description: None,
    };

    let scan = scan_replacement(
        WORKER,
        &committed,
        Some(&declared),
        &InitialSiteProbeReport::default(),
        "amd64",
    )
    .expect("the node's scan");

    assert_eq!(
        scan.unavailable,
        vec![(
            RescanCapability::DockerApplications,
            vec![docker_application_target(WORKER)],
        )]
    );
    // The kept target stays exactly as declared, and the replacement covers it once.
    assert_eq!(
        scan.targets
            .iter()
            .filter(|target| target.execution_target_id
                == docker_application_target(WORKER).execution_target_id)
            .count(),
        1
    );

    let rules = crate::test_support::site_declaration_rules();
    let replacement = derive_replacement(&site, &rules, WORKER, scan.targets.clone(), Some(&base))
        .expect("a replacement keeping an unavailable target is valid");
    assert!(
        replacement
            .targets()
            .iter()
            .any(|target| target.runtime_binding
                == docker_application_target(WORKER).runtime_binding)
    );
}

/// What probing a Mac with Docker Desktop running reports: its Apple GPU, and Docker running Linux
/// on the Mac's architecture.
fn mac_with_docker() -> InitialSiteProbeReport {
    InitialSiteProbeReport::described(&MachineDescription::named("mac-64g").expect("a named Mac"))
        .expect("a probe of the Mac")
}

/// Returns the worker site with the worker declared as a Mac set up without Docker: one native
/// model target on its Apple GPU and no application target.
fn native_only_worker() -> (InitializedSite, Site) {
    let (fixture, site) = worker_site();
    let request = InitialSiteRequest::new([InitialInferenceRuntime::Apple], []);
    let choices = NodeTargetChoices::resolve(&request, &mac_with_docker(), "aarch64")
        .expect("the native-only targets");
    (
        fixture,
        with_worker_targets(site, build_execution_targets(WORKER, &choices)),
    )
}

/// Returns the native-only worker site after a re-scan on the Mac with Docker committed its
/// replacement: the native model target and the added Docker application target.
fn docker_gained_worker() -> (InitializedSite, Site) {
    let (fixture, site) = native_only_worker();
    let scan = scan_replacement(
        WORKER,
        &committed(&site),
        None,
        &mac_with_docker(),
        "aarch64",
    )
    .expect("the node's scan");
    (fixture, with_worker_targets(site, scan.targets))
}

/// Returns `site` with the worker's targets replaced by `targets`, as a committed re-scan leaves it.
fn with_worker_targets(mut site: Site, targets: Vec<ExecutionTarget>) -> Site {
    site.execution_targets
        .retain(|target| target.site_node_id != WORKER);
    site.execution_targets.extend(targets);
    site
}

/// Returns the capability of each target, in order.
fn capabilities(targets: &[ExecutionTarget]) -> Vec<RescanCapability> {
    targets
        .iter()
        .map(|target| RescanCapability::of(target).expect("a scanned capability"))
        .collect()
}
