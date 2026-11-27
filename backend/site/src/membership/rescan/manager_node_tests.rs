//! The manager node's own re-scan, held in its real site folder: what a confirmed replacement
//! changes, what a whole projection needing no bytes leaves untouched, and what a refused or failed
//! write leaves behind.

use std::{fs, path::PathBuf};

use dotcore_proto::dotcore::{
    catalog::v1::ModelParallelShape,
    deployment::v1::ExactPlacement,
    inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentLifecycleTransition,
    },
    site::v1::{ExecutionTarget, Site},
};
use dotcore_proto::yaml;

use super::{
    ManagerNodeRescan, ManagerNodeRescanError, ManagerRescanOutcome, RescanBase, RescanCommitError,
    RescanDeploymentImpact, RescanError, RescanReplacement,
    commit::replace_targets,
    commit::{ReplacementChangeError, ReplacementRole},
    derive_replacement, record_replacement, rescan_base,
};
use crate::{
    NODE_FILE, SITE_FILE,
    initial::{
        input_store::try_acquire_exclusive_site_lock, probe::InitialSiteProbeReport,
        validation::SiteInputBundleValidator,
    },
    membership::join::test_fixtures::{
        InitializedSite, initialized_site, worker_node, worker_targets,
    },
    membership::removal::DeploymentKind,
    membership::rescan::test_fixtures::target_by_role,
    repository::{MembershipSiteTransaction, SiteRepository, SiteWriteFault},
    test_support::site_declaration_rules,
    validation::load_validated_site,
};

/// The worker node the fixture site declares next to its manager.
const WORKER: &str = "node-worker";

/// The name a replacement gives the manager node's model target, which identifies the recorded
/// replacement in the site file.
const RENEWED: &str = "Renewed model target";

/// A manager site folder that also declares a worker with its two targets, a model deployment on
/// the worker's model target, and the route serving it: the declarations a manager re-scan must
/// leave untouched. The kept `InitializedSite` holds the fixture's temporary directory, and the
/// returned site is what `site.yaml` holds.
fn manager_site() -> (InitializedSite, Site) {
    let fixture = initialized_site();
    let mut site = fixture.site.clone();
    site.nodes.push(worker_node(WORKER, "Worker", "10.0.0.7"));
    site.execution_targets.extend(worker_targets(WORKER));
    site.model_deployments.push(model_deployment(
        "worker-model",
        WORKER,
        &format!("{WORKER}-model-target"),
    ));
    site.inference_routes
        .push(route("route-worker", "worker-model"));
    write_site(&fixture, site.clone());
    (fixture, site)
}

/// Declares `site` in `fixture`'s folder, as the running manager would have written it.
fn write_site(fixture: &InitializedSite, site: Site) {
    let repository = SiteRepository::load(&fixture.root.join(SITE_FILE), site_declaration_rules())
        .expect("open the fixture site");
    repository.replace(site).expect("declare the fixture site");
}

/// The identity of the site's manager node.
fn manager_id(site: &Site) -> String {
    site.nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .expect("a manager node")
        .node_id
        .clone()
}

/// The manager node's execution targets, in site order.
fn manager_targets(site: &Site) -> Vec<ExecutionTarget> {
    let manager = manager_id(site);
    site.execution_targets
        .iter()
        .filter(|target| target.site_node_id == manager)
        .cloned()
        .collect()
}

/// A model deployment placed on `target_id` of `node_id`, present and serving.
fn model_deployment(id: &str, node_id: &str, target_id: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: id.to_owned(),
        model_deployment_name: format!("{id} deployment"),
        placement: Some(ExactPlacement {
            site_node_id: node_id.to_owned(),
            execution_target_id: target_id.to_owned(),
        }),
        requested_parallel_shape: ModelParallelShape::One as i32,
        snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
            "fixture-model",
        )),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        lifecycle_generation: 1,
        lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
        ..Default::default()
    }
}

/// An inference route serving the model deployment `deployment_id`.
fn route(id: &str, deployment_id: &str) -> InferenceRoute {
    InferenceRoute {
        inference_route_id: id.to_owned(),
        name: id.to_owned(),
        enabled: true,
        fallback_groups: vec![FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![deployment_id.to_owned()],
        }],
        generation: 1,
    }
}

/// Opens the manager site folder as this machine's re-scan command would.
fn open(fixture: &InitializedSite) -> ManagerNodeRescan {
    ManagerNodeRescan::open(&fixture.root, &SiteInputBundleValidator::for_test())
        .expect("open the manager site folder")
}

/// Scans this machine and plans the replacement that renames the manager node's model target: the
/// whole projection an operator confirms.
fn plan_renewed_model_target(rescan: &ManagerNodeRescan) -> RescanReplacement {
    let scan = rescan
        .scan(&InitialSiteProbeReport::default())
        .expect("scan this machine");
    let model = target_by_role(rescan.committed_targets(), "model");
    let mut targets = scan.targets;
    targets
        .iter_mut()
        .find(|target| target.execution_target_id == model.execution_target_id)
        .expect("the scanned model target")
        .display_name = RENEWED.to_owned();
    rescan.plan(targets).expect("plan the replacement")
}

/// The site file's path.
fn site_file(fixture: &InitializedSite) -> PathBuf {
    fixture.root.join(SITE_FILE)
}

/// The site file's exact bytes.
fn site_bytes(fixture: &InitializedSite) -> String {
    fs::read_to_string(site_file(fixture)).expect("site file")
}

/// The site `site.yaml` declares.
fn durable_site(fixture: &InitializedSite) -> Site {
    load_validated_site(&site_file(fixture), &site_declaration_rules()).expect("site")
}

#[test]
fn the_site_folder_re_scans_its_own_manager_node() {
    let (fixture, site) = manager_site();

    let rescan = open(&fixture);

    assert_eq!(rescan.site_id(), site.site_id);
    assert_eq!(rescan.node_id(), manager_id(&site));
    assert_eq!(rescan.site_root(), fixture.root);
    // The base is the manager node's own committed targets: no other node's targets join them.
    assert_eq!(
        rescan.committed_targets(),
        manager_targets(&site).as_slice()
    );
    assert!(!rescan.committed_targets().is_empty());
}

#[test]
fn an_unchanged_machine_writes_no_bytes_and_needs_no_restart() {
    let (fixture, site) = manager_site();
    let before = site_bytes(&fixture);
    let rescan = open(&fixture);

    let scan = rescan
        .scan(&InitialSiteProbeReport::default())
        .expect("scan this machine");
    let plan = rescan.plan(scan.targets).expect("plan the replacement");
    let outcome = rescan.commit(&plan).expect("commit the replacement");

    assert!(
        scan.unavailable.is_empty(),
        "this machine still offers every capability the manager node declares"
    );
    assert!(plan.is_noop());
    assert!(plan.classification().is_unchanged());
    assert_eq!(outcome, ManagerRescanOutcome::Unchanged);
    assert_eq!(
        site_bytes(&fixture),
        before,
        "a whole projection that changes nothing writes no bytes"
    );
    assert_eq!(durable_site(&fixture), site);
}

#[test]
fn a_confirmed_replacement_changes_only_the_manager_node_targets() {
    let (fixture, site) = manager_site();
    let rescan = open(&fixture);

    let plan = plan_renewed_model_target(&rescan);
    let outcome = rescan.commit(&plan).expect("commit the replacement");

    assert_eq!(outcome, ManagerRescanOutcome::Recorded);
    assert_eq!(
        plan.classification()
            .changed
            .iter()
            .map(|target| target.display_name.as_str())
            .collect::<Vec<_>>(),
        [RENEWED]
    );
    assert!(plan.classification().added.is_empty());
    assert!(plan.classification().removed.is_empty());

    // The site file is the fixture site with the manager node's whole target set replaced, so the
    // worker, its targets, both deployments and both routes stay byte for byte as they were.
    let manager = manager_id(&site);
    let mut expected = site.clone();
    expected
        .execution_targets
        .retain(|target| target.site_node_id != manager);
    expected
        .execution_targets
        .extend(plan.targets().iter().cloned());
    assert_eq!(
        yaml::to_string(&expected).expect("expected site"),
        site_bytes(&fixture),
        "only the manager node's targets differ"
    );

    // The manager node's own declaration is untouched, and only its model target changed.
    let after = durable_site(&fixture);
    assert_eq!(after.nodes, site.nodes);
    assert_eq!(
        after
            .execution_targets
            .iter()
            .filter(|target| target.site_node_id == manager)
            .map(|target| target.execution_target_id.as_str())
            .collect::<Vec<_>>(),
        manager_targets(&site)
            .iter()
            .map(|target| target.execution_target_id.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_failed_write_leaves_the_targets_the_site_had() {
    let (fixture, site) = manager_site();
    let before = site_bytes(&fixture);
    let (rescan, faults) = ManagerNodeRescan::open_with_write_faults(
        &fixture.root,
        &SiteInputBundleValidator::for_test(),
    )
    .expect("open the manager site folder for a faulted write");

    let plan = plan_renewed_model_target(&rescan);
    faults.push(SiteWriteFault::Storage);
    let error = rescan.commit(&plan).expect_err("the write fails");

    assert!(
        matches!(
            error,
            ManagerNodeRescanError::Commit(RescanCommitError::Failed(_))
        ),
        "{error}"
    );
    assert!(faults.attempts() > 0, "the replacement was written once");
    assert_eq!(
        site_bytes(&fixture),
        before,
        "a failed write leaves the prior targets"
    );
    assert_eq!(durable_site(&fixture), site);
    assert!(
        durable_site(&fixture)
            .execution_targets
            .iter()
            .all(|target| target.display_name != RENEWED),
        "the failed replacement is not declared anywhere"
    );
}

#[test]
fn an_uncertain_write_that_landed_is_recorded_and_not_rolled_back() {
    let (fixture, _site) = manager_site();
    let (rescan, faults) = ManagerNodeRescan::open_with_write_faults(
        &fixture.root,
        &SiteInputBundleValidator::for_test(),
    )
    .expect("open the manager site folder for a faulted write");

    let plan = plan_renewed_model_target(&rescan);
    // The replacement reaches the file, its confirmation is lost, and writing the prior site back
    // fails too, so only comparing the durable declaration decides the outcome.
    faults.push(SiteWriteFault::Uncertain);
    faults.push(SiteWriteFault::Storage);
    let outcome = rescan
        .commit(&plan)
        .expect("the durable declaration decides");

    assert_eq!(outcome, ManagerRescanOutcome::Recorded);
    assert!(
        durable_site(&fixture)
            .execution_targets
            .iter()
            .any(|target| target.display_name == RENEWED),
        "a recorded replacement is never rolled back"
    );
}

#[test]
fn the_deployment_guard_refuses_replacing_a_target_that_carries_one() {
    let (fixture, site) = manager_site();
    let guarded = target_by_role(&manager_targets(&site), "model").execution_target_id;
    // A deployment is placed on one of the manager's own targets, which the guard then guards.
    let mut guarded_site = site.clone();
    guarded_site.model_deployments.push(model_deployment(
        "manager-model",
        &manager_id(&site),
        &guarded,
    ));
    guarded_site
        .inference_routes
        .push(route("route-manager", "manager-model"));
    write_site(&fixture, guarded_site);
    let before = site_bytes(&fixture);

    let rescan = open(&fixture);
    let scan = rescan
        .scan(&InitialSiteProbeReport::default())
        .expect("scan this machine");
    // The replacement drops the guarded target, so the whole replacement is refused.
    let proposed = scan
        .targets
        .iter()
        .filter(|target| target.execution_target_id != guarded)
        .cloned()
        .collect::<Vec<_>>();

    let error = rescan.plan(proposed).expect_err("the guard refuses");

    let ManagerNodeRescanError::Refused(RescanError::DeploymentGuard { impacts }) = error else {
        panic!("expected a deployment guard refusal: {error}");
    };
    assert_eq!(
        impacts
            .iter()
            .map(|impact| (
                impact.deployment_id.as_str(),
                impact.execution_target_id.as_str()
            ))
            .collect::<Vec<_>>(),
        [("manager-model", guarded.as_str())]
    );
    assert_eq!(
        site_bytes(&fixture),
        before,
        "a refused replacement leaves the site as it was"
    );
}

#[test]
fn an_invalid_proposal_is_refused_and_writes_nothing() {
    let (fixture, _site) = manager_site();
    let before = site_bytes(&fixture);
    let rescan = open(&fixture);

    // A target naming another node is not this node's replacement.
    let mut proposed = rescan.committed_targets().to_vec();
    proposed[0].site_node_id = WORKER.to_owned();
    let error = rescan.plan(proposed).expect_err("the proposal is invalid");

    assert!(
        matches!(
            error,
            ManagerNodeRescanError::Refused(RescanError::ForeignTarget { .. })
        ),
        "{error}"
    );
    assert_eq!(
        site_bytes(&fixture),
        before,
        "a refused proposal writes nothing"
    );
}

#[test]
fn a_site_held_by_its_running_manager_is_refused_before_anything_changes() {
    let (fixture, _site) = manager_site();
    let held = try_acquire_exclusive_site_lock(&fixture.root)
        .expect("hold the site lock")
        .expect("no site command holds it yet");

    let error = ManagerNodeRescan::open(&fixture.root, &SiteInputBundleValidator::for_test())
        .err()
        .expect("the running manager holds the site");

    assert!(
        matches!(error, ManagerNodeRescanError::SiteRunning { .. }),
        "{error}"
    );
    drop(held);
    // With the site free, the same folder opens for its manager node.
    assert!(ManagerNodeRescan::open(&fixture.root, &SiteInputBundleValidator::for_test()).is_ok());
}

#[test]
fn a_folder_holding_no_site_names_the_folder_the_re_scan_needs() {
    let directory = dotcore_domain::test_support::canonical_tempdir().expect("temporary folder");

    let error = ManagerNodeRescan::open(directory.path(), &SiteInputBundleValidator::for_test())
        .err()
        .expect("no site here");

    assert!(
        matches!(error, ManagerNodeRescanError::NotSiteFolder { .. }),
        "{error}"
    );
}

#[test]
fn a_folder_binding_a_worker_node_is_not_a_manager_re_scan() {
    let (fixture, site) = manager_site();
    fs::write(fixture.root.join(NODE_FILE), format!("nodeId: {WORKER}\n"))
        .expect("node configuration naming the worker");

    let error = ManagerNodeRescan::open(&fixture.root, &SiteInputBundleValidator::for_test())
        .err()
        .expect("this folder binds a worker");

    assert!(
        matches!(&error, ManagerNodeRescanError::NotManagerNode { node_id } if node_id.as_str() == WORKER),
        "{error}"
    );
    assert_eq!(durable_site(&fixture), site, "the site is left as it was");
}

#[test]
fn a_replacement_is_recorded_only_for_the_declaration_its_role_names() {
    let (_fixture, site) = manager_site();
    let manager = manager_id(&site);
    let rules = site_declaration_rules();

    // A replacement approved for this site's manager node.
    let manager_base = rescan_base(&site, &manager).expect("manager base");
    let for_manager = derive_replacement(
        &site,
        &rules,
        &manager,
        manager_targets(&site),
        Some(&manager_base),
    )
    .expect("replacement for the manager node");
    // A replacement approved for the site's joined worker.
    let worker_base = rescan_base(&site, WORKER).expect("worker base");
    let for_worker = derive_replacement(
        &site,
        &rules,
        WORKER,
        worker_targets(WORKER),
        Some(&worker_base),
    )
    .expect("replacement for the worker node");

    // A worker's re-scan never reaches the manager node's targets.
    assert!(
        matches!(
            recorded(&site, &manager_base, &for_manager, ReplacementRole::Worker),
            Err(ReplacementChangeError::NodeNotDeclared { .. })
        ),
        "a worker replacement does not touch the manager node"
    );
    // A manager node's re-scan never reaches a worker's targets.
    assert!(
        matches!(
            recorded(
                &site,
                &worker_base,
                &for_worker,
                ReplacementRole::SiteManager
            ),
            Err(ReplacementChangeError::ManagerNotDeclared { .. })
        ),
        "a manager replacement does not touch a worker node"
    );
    // Under the site lock the node must still hold the base that was confirmed.
    let mut moved_base = manager_base.clone();
    moved_base
        .targets
        .push(target_by_role(&manager_targets(&site), "application"));
    assert!(
        matches!(
            recorded(
                &site,
                &moved_base,
                &for_manager,
                ReplacementRole::SiteManager
            ),
            Err(ReplacementChangeError::StaleBase)
        ),
        "a base that moved is refused"
    );
    // Each role records its own node's replacement.
    assert!(
        recorded(
            &site,
            &manager_base,
            &for_manager,
            ReplacementRole::SiteManager
        )
        .is_ok(),
        "the manager role records the manager node's replacement"
    );
    assert!(
        recorded(&site, &worker_base, &for_worker, ReplacementRole::Worker).is_ok(),
        "the worker role records the worker node's replacement"
    );
}

/// A deployment placed on a target the replacement changes, after the replacement was derived and
/// before it is written, refuses the whole replacement under the site lock and writes nothing.
#[test]
fn a_deployment_placed_after_the_review_refuses_the_replacement_under_the_site_lock() {
    let (fixture, site) = manager_site();
    let manager = manager_id(&site);
    let base = rescan_base(&site, &manager).expect("manager base");
    let model = target_by_role(&manager_targets(&site), "model");
    let mut targets = manager_targets(&site);
    targets
        .iter_mut()
        .find(|target| target.execution_target_id == model.execution_target_id)
        .expect("the manager node's model target")
        .display_name = RENEWED.to_owned();
    let replacement = derive_replacement(
        &site,
        &site_declaration_rules(),
        &manager,
        targets,
        Some(&base),
    )
    .expect("the replacement the operator confirmed");

    let mut placed = site.clone();
    placed.model_deployments.push(model_deployment(
        "manager-model",
        &manager,
        &model.execution_target_id,
    ));
    write_site(&fixture, placed.clone());
    let before = site_bytes(&fixture);
    let repository = SiteRepository::load(&site_file(&fixture), site_declaration_rules())
        .expect("open the site");
    let error = record_replacement(
        &repository,
        &base,
        &replacement,
        ReplacementRole::SiteManager,
    )
    .expect_err("the placed deployment refuses the replacement");

    let RescanCommitError::Rejected(source) = &error else {
        panic!("expected the site to refuse the replacement, saw {error}");
    };
    assert!(
        matches!(
            source.downcast_ref::<ReplacementChangeError>(),
            Some(ReplacementChangeError::DeploymentGuard { impacts })
                if impacts.as_slice() == [RescanDeploymentImpact {
                    kind: DeploymentKind::Model,
                    deployment_id: "manager-model".to_owned(),
                    name: "manager-model deployment".to_owned(),
                    execution_target_id: model.execution_target_id.clone(),
                    tearing_down: false,
                }]
        ),
        "{error}"
    );
    assert_eq!(site_bytes(&fixture), before, "nothing is written");
    assert_eq!(repository.snapshot().expect("the running site"), placed);
}

/// Runs the site-lock half of a replacement write against a copy of `site`, which is what the
/// repository hands the writer under that lock.
fn recorded(
    site: &Site,
    base: &RescanBase,
    replacement: &RescanReplacement,
    role: ReplacementRole,
) -> Result<(), ReplacementChangeError> {
    let mut candidate = site.clone();
    let mut transaction = MembershipSiteTransaction {
        nodes: &mut candidate.nodes,
        execution_targets: &mut candidate.execution_targets,
        deployments: &candidate.model_deployments,
        applications: &candidate.application_deployments,
    };
    replace_targets(&mut transaction, base, replacement, role)
}
