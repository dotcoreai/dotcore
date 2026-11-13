use std::{
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, Mutex},
    time::{Duration, Instant},
};

use super::{
    DirectoryTransactionFileSystem, InitialSiteInputWriter, InitialSitePresence,
    InitialSiteWriteError, InitialSiteWriteOutcome, SITE_FILE, SiteInputLockError,
    SiteRepositoryError, StdDirectoryTransactionFileSystem, acquire_shared_site_lock, backup_path,
    reject_provisioned_worker_topology_change,
};
use crate::initial::{
    request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
    validation::SiteInputBundleValidator,
};
use crate::test_support::initial_site_inputs;

#[test]
fn creates_only_validated_site_and_node_inputs() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");

    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&root, &initial_site_inputs())
        .expect("new site inputs");

    assert!(root.join("site.yaml").is_file());
    assert!(root.join("node.yaml").is_file());
    assert_eq!(
        directory_entries(&root),
        ["node.yaml", "site.yaml", "trust"]
    );
}

#[test]
fn simultaneous_site_roots_are_distinct_and_independently_owned() {
    let first = SiteRoot::new();
    let second = SiteRoot::new();
    let first_path = first.root.clone();
    let second_marker = second.root.join("retained");
    fs::write(&second_marker, b"second fixture").expect("second fixture marker");

    assert_ne!(first.root, second.root);
    drop(first);

    assert!(!first_path.exists());
    assert_eq!(
        fs::read(&second_marker).expect("retained second fixture marker"),
        b"second fixture"
    );
}

#[test]
fn if_missing_inspection_accepts_an_absent_parent_without_mutation() {
    let fixture = SiteRoot::new();
    let parent = fixture.root.join("missing-parent");
    let site_root = parent.join("site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    assert!(matches!(
        writer
            .inspect_if_missing(&site_root)
            .expect("inspect site with missing parent"),
        InitialSitePresence::Missing
    ));
    assert!(!parent.exists());
    assert!(!PathBuf::from(format!("{}.lock", site_root.display())).exists());

    fs::create_dir(&parent).expect("create site parent");
    let backup = parent.join(".site.init-backup-site-0");
    fs::write(&backup, "recoverable").expect("create recovery backup");
    assert!(matches!(
        writer
            .inspect_if_missing(&site_root)
            .expect_err("recovery backup must be rejected"),
        InitialSiteWriteError::RecoverableBackup { path, .. } if path == backup
    ));
    assert_eq!(fs::read(&backup).expect("backup bytes"), b"recoverable");
    assert!(!site_root.exists());

    let not_directory = fixture.root.join("not-a-directory");
    fs::write(&not_directory, "unchanged").expect("create non-directory parent");
    assert!(matches!(
        writer
            .inspect_if_missing(&not_directory.join("site"))
            .expect_err("non-directory parent must be rejected"),
        InitialSiteWriteError::Storage(_)
    ));
    assert_eq!(
        fs::read(&not_directory).expect("non-directory bytes"),
        b"unchanged"
    );
}

#[test]
fn if_missing_inspection_is_read_only_and_rejects_invalid_or_running_sites() {
    let fixture = SiteRoot::new();
    let missing = fixture.root.join("missing-site");
    assert!(matches!(
        InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
            .inspect_if_missing(&missing)
            .expect("missing site inspection"),
        InitialSitePresence::Missing
    ));
    assert!(!missing.exists());
    assert!(!PathBuf::from(format!("{}.lock", missing.display())).exists());

    let valid = fixture.root.join("valid-site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer
        .create(&valid, &initial_site_inputs())
        .expect("create valid site");
    let site_before = fs::read(valid.join("site.yaml")).expect("Site bytes");
    let node_before = fs::read(valid.join("node.yaml")).expect("node bytes");
    assert!(matches!(
        writer
            .inspect_if_missing(&valid)
            .expect("valid site inspection"),
        InitialSitePresence::Initialized
    ));
    assert_eq!(
        fs::read(valid.join("site.yaml")).expect("Site bytes after"),
        site_before
    );
    assert_eq!(
        fs::read(valid.join("node.yaml")).expect("node bytes after"),
        node_before
    );

    let running_lock = acquire_shared_site_lock(&valid, Duration::ZERO).expect("running site lock");
    assert!(matches!(
        writer.inspect_if_missing(&valid).expect_err("running site"),
        InitialSiteWriteError::SiteRunning
    ));
    drop(running_lock);

    let partial = fixture.root.join("partial-site");
    fs::create_dir(&partial).expect("partial site root");
    fs::write(partial.join("site.yaml"), &site_before).expect("partial site file");
    let partial_before = fs::read(partial.join("site.yaml")).expect("partial bytes");
    assert!(matches!(
        writer
            .inspect_if_missing(&partial)
            .expect_err("partial site"),
        InitialSiteWriteError::ExistingInputsInvalid(_)
    ));
    assert_eq!(
        fs::read(partial.join("site.yaml")).expect("partial bytes after"),
        partial_before
    );
    assert!(!PathBuf::from(format!("{}.lock", partial.display())).exists());
}

#[test]
fn clean_create_durably_creates_a_missing_parent_and_rejects_symlink_paths() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("missing-parent/site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&root, &initial_site_inputs())
        .expect("create with missing parent");
    assert!(root.join(SITE_FILE).is_file());

    let external = fixture.root.join("external");
    fs::create_dir(&external).expect("external directory");
    let linked_parent = fixture.root.join("linked-parent");
    symlink(&external, &linked_parent).expect("symlink ancestor");
    let ancestor_error = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&linked_parent.join("site"), &initial_site_inputs())
        .expect_err("symlink ancestor refusal");
    assert!(matches!(ancestor_error, InitialSiteWriteError::Lock(_)));
    assert!(!external.join("site").exists());

    let lock_target = fixture.root.join("lock-target");
    fs::write(&lock_target, "unchanged").expect("lock target");
    let lock_site = fixture.root.join("locked-site");
    symlink(
        &lock_target,
        PathBuf::from(format!("{}.lock", lock_site.display())),
    )
    .expect("symlink lock");
    let lock_error = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&lock_site, &initial_site_inputs())
        .expect_err("symlink lock refusal");
    assert!(matches!(lock_error, InitialSiteWriteError::Lock(_)));
    assert_eq!(
        fs::read_to_string(lock_target).expect("lock target"),
        "unchanged"
    );
}

#[test]
fn concurrent_matching_initializations_are_one_idempotent_transaction() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = Arc::new(InitialSiteInputWriter::new(
        SiteInputBundleValidator::for_test(),
    ));
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let writer = Arc::clone(&writer);
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        threads.push(std::thread::spawn(move || {
            let request = InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Simulated],
            );
            let inputs = initial_site_inputs();
            barrier.wait();
            writer.initialize(&root, &inputs, &request, false)
        }));
    }
    let outcomes = threads
        .into_iter()
        .map(|thread| {
            thread
                .join()
                .expect("initialization thread")
                .expect("transaction")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, InitialSiteWriteOutcome::Published))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, InitialSiteWriteOutcome::AlreadyInitialized(_)))
            .count(),
        1
    );
    assert_eq!(
        directory_entries(&root),
        ["node.yaml", "site.yaml", "trust"]
    );
}

#[test]
fn matching_initialization_and_input_replacement_preserve_trust() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let inputs = initial_site_inputs();
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer.create(&root, &inputs).expect("initial site");
    let trust_anchor = fs::read(root.join("trust/site-ca.der")).expect("trust anchor");
    let manager_certificate = fs::read(root.join("trust/node.der")).expect("manager node identity");
    let request = InitialSiteRequest::new(
        [InitialInferenceRuntime::Simulated],
        [InitialApplicationRuntime::Simulated],
    );

    assert!(matches!(
        writer
            .initialize(&root, &inputs, &request, false)
            .expect("matching site"),
        InitialSiteWriteOutcome::AlreadyInitialized(_)
    ));
    writer.replace(&root, &inputs).expect("replace site inputs");

    assert_eq!(
        fs::read(root.join("trust/site-ca.der")).unwrap(),
        trust_anchor
    );
    assert_eq!(
        fs::read(root.join("trust/node.der")).unwrap(),
        manager_certificate
    );
    fs::remove_dir(root.join("trust/worker-pins")).expect("remove empty registry");
    assert!(matches!(
        writer.initialize(&root, &inputs, &request, false),
        Err(InitialSiteWriteError::ExistingTrustInvalid(_))
    ));
    assert!(matches!(
        writer.replace(&root, &inputs),
        Err(InitialSiteWriteError::ExistingTrustInvalid(_))
    ));
}

#[test]
fn replacement_refuses_state_a_backend_run_left_and_otherwise_keeps_trust() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let inputs = initial_site_inputs();
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer.create(&root, &inputs).expect("initial site");
    let trust_anchor = fs::read(root.join("trust/site-ca.der")).expect("trust anchor");
    let site_before = fs::read(root.join(SITE_FILE)).expect("Site bytes");
    let request = InitialSiteRequest::new(
        [InitialInferenceRuntime::Simulated],
        [InitialApplicationRuntime::Simulated],
    );

    for (name, directory) in [
        ("state.pb", false),
        ("node", true),
        ("applications-data", true),
        ("logs", true),
        ("telemetry", true),
    ] {
        let path = root.join(name);
        if directory {
            fs::create_dir(&path).expect("retained directory");
        } else {
            fs::write(&path, b"retained").expect("retained file");
        }

        let result = writer.initialize(&root, &inputs, &request, true);

        assert!(
            matches!(
                result,
                Err(InitialSiteWriteError::RetainedStateResetRequired)
            ),
            "{name} must require a reset"
        );
        assert_eq!(fs::read(root.join(SITE_FILE)).unwrap(), site_before);
        if directory {
            fs::remove_dir(&path).expect("remove retained directory");
        } else {
            fs::remove_file(&path).expect("remove retained file");
        }
    }

    assert!(matches!(
        writer
            .initialize(&root, &inputs, &request, true)
            .expect("input-only replacement"),
        InitialSiteWriteOutcome::Published
    ));
    assert_eq!(
        fs::read(root.join("trust/site-ca.der")).unwrap(),
        trust_anchor
    );
}

#[test]
fn replacement_rejects_provisioned_worker_topology_without_mutating_site_or_trust() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    let inputs = initial_site_inputs();
    writer.create(&root, &inputs).expect("initial site");

    let site_path = root.join(SITE_FILE);
    let mut site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("read site");
    let mut worker = site.nodes[0].clone();
    worker.node_id = "worker-one".to_owned();
    worker.host = "127.0.0.2".to_owned();
    worker.display_name = "Worker one".to_owned();
    worker.site_manager = None;
    site.nodes.push(worker);
    let mut target = site.execution_targets[0].clone();
    target.execution_target_id = "worker-one-target".to_owned();
    target.site_node_id = "worker-one".to_owned();
    site.execution_targets.push(target);
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("serialize provisioned site"),
    )
    .expect("write provisioned site");
    fs::write(
        root.join("trust/worker-pins/worker-one.sha256"),
        b"provisioned-worker-pin\n",
    )
    .expect("worker pin");
    let site_before = fs::read(&site_path).expect("Site bytes");
    let trust_before = trust_bytes(&root);

    let error = writer
        .replace(&root, &inputs)
        .expect_err("provisioned topology replacement refusal");

    assert!(matches!(
        error,
        InitialSiteWriteError::ProvisionedWorkerTopology
    ));
    assert_eq!(fs::read(&site_path).expect("Site bytes after"), site_before);
    assert_eq!(trust_bytes(&root), trust_before);
}

#[test]
fn topology_guard_covers_each_pinned_node_manager_host() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&root, &initial_site_inputs())
        .expect("initial site");
    let mut existing: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&root.join(SITE_FILE)).expect("read site");
    let mut worker = existing.nodes[0].clone();
    worker.node_id = "worker-one".to_owned();
    worker.host = "127.0.0.2".to_owned();
    worker.display_name = "Worker one".to_owned();
    worker.site_manager = None;
    existing.nodes.push(worker);
    let mut target = existing.execution_targets[0].clone();
    target.execution_target_id = "worker-one-target".to_owned();
    target.site_node_id = "worker-one".to_owned();
    existing.execution_targets.push(target);
    fs::write(
        root.join("trust/worker-pins/worker-one.sha256"),
        b"provisioned-worker-pin\n",
    )
    .expect("worker pin");

    reject_provisioned_worker_topology_change(&root, &existing, &existing.clone())
        .expect("unchanged topology");
    let mut moved = existing.clone();
    moved.nodes[1].manager_host = "100.64.0.10".to_owned();
    assert!(matches!(
        reject_provisioned_worker_topology_change(&root, &existing, &moved),
        Err(InitialSiteWriteError::ProvisionedWorkerTopology)
    ));
}

#[test]
fn replacement_rejects_controller_owned_deployment_declarations() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer
        .create(&root, &initial_site_inputs())
        .expect("initial site");
    let site_path = root.join(SITE_FILE);
    let original = fs::read_to_string(&site_path).expect("site YAML");
    for declaration in [
        "modelDeployments:\n- modelDeploymentId: model-test\n",
        "applicationDeployments:\n- applicationDeploymentId: app-test\n",
        "inferenceRoutes:\n- inferenceRouteId: route-test\n",
    ] {
        let declared = format!("{original}{declaration}");
        fs::write(&site_path, &declared).expect("controller declaration");
        let error = writer
            .replace(&root, &initial_site_inputs())
            .expect_err("deployment replacement refusal");
        assert!(matches!(
            error,
            InitialSiteWriteError::DeploymentDeclarations
        ));
        assert_eq!(
            fs::read_to_string(&site_path).expect("preserved site YAML"),
            declared
        );
    }
}

#[test]
fn replacement_names_the_schema_mismatch_and_the_reset_for_an_outdated_site_yaml() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer
        .create(&root, &initial_site_inputs())
        .expect("initial site");
    let site_path = root.join(SITE_FILE);
    let outdated = format!(
        "{}applicationDeployments:\n- applicationDeploymentId: app-test\n  snapshot:\n    serviceSelections:\n    - serviceName: web\n      preparationPolicy: APPLICATION_ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT\n",
        fs::read_to_string(&site_path).expect("site YAML")
    );
    fs::write(&site_path, &outdated).expect("outdated site YAML");

    let error = writer
        .replace(&root, &initial_site_inputs())
        .expect_err("outdated site YAML");

    assert!(matches!(
        error,
        InitialSiteWriteError::DeploymentInspection(SiteRepositoryError::SchemaMismatch(_))
    ));
    assert_eq!(
        error.to_string(),
        "existing site deployment declarations could not be inspected: site.yaml does not match the site schema of this build (unknown variant `APPLICATION_ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT`, expected one of `ARTIFACT_PREPARATION_POLICY_UNSPECIFIED`, `ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE`, `ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT`, `ARTIFACT_PREPARATION_POLICY_DEVELOPMENT_SOURCE`); correct the named value or reset the site"
    );
    assert_eq!(
        fs::read_to_string(&site_path).expect("preserved site YAML"),
        outdated
    );
}

#[test]
fn validation_failure_never_makes_a_new_target_visible() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = InitialSiteInputWriter::new(
        SiteInputBundleValidator::for_test()
            .with_platform_catalog_root(fixture.root.join("missing-platform-catalog")),
    );

    let error = writer
        .create(&root, &initial_site_inputs())
        .expect_err("platform catalog validation");

    assert!(matches!(error, InitialSiteWriteError::InvalidInputs(_)));
    assert!(!root.exists());
    assert!(
        directory_entries(&fixture.root)
            .iter()
            .all(|entry| { entry == "site.lock" || !entry.starts_with(".site.init-stage-") })
    );
}

#[test]
fn retained_state_requires_explicit_reset_confirmation() {
    let error = InitialSiteWriteError::RetainedStateResetRequired;

    assert_eq!(
        error.to_string(),
        "retained site state requires explicit reset confirmation"
    );
}

#[test]
fn preserves_existing_targets_and_replaces_only_exact_owned_inputs() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer
        .create(&root, &initial_site_inputs())
        .expect("initial site");

    let create_error = writer
        .create(&root, &initial_site_inputs())
        .expect_err("preserve site");
    assert!(matches!(create_error, InitialSiteWriteError::TargetExists));

    writer
        .replace(&root, &initial_site_inputs())
        .expect("guarded replacement");
    fs::create_dir(root.join("catalog")).expect("non-owned catalog");
    writer
        .replace(&root, &initial_site_inputs())
        .expect("preserve non-owned catalog");
    assert!(root.join("catalog").is_dir());
}

#[test]
fn rejects_symlinks_unknown_state_and_recoverable_backups_without_mutation() {
    let fixture = SiteRoot::new();
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    let unknown = fixture.root.join("unknown");
    writer
        .create(&unknown, &initial_site_inputs())
        .expect("initial site");
    fs::write(unknown.join("state-not-interpreted"), "retained state").expect("state");

    let unknown_error = writer
        .replace(&unknown, &initial_site_inputs())
        .expect_err("unknown state");
    assert!(matches!(
        unknown_error,
        InitialSiteWriteError::UnexpectedEntry { .. }
    ));

    let recovery = fixture.root.join("recovery");
    writer
        .create(&recovery, &initial_site_inputs())
        .expect("initial site");
    let backup = fixture.root.join(".recovery.init-backup-test");
    fs::rename(&recovery, &backup).expect("simulated crash backup");
    let recovery_error = writer
        .create(&recovery, &initial_site_inputs())
        .expect_err("backup recovery");
    assert!(matches!(
        recovery_error,
        InitialSiteWriteError::RecoverableBackup { .. }
    ));

    let symlink_root = fixture.root.join("symlink");
    symlink(&backup, &symlink_root).expect("Site root symlink");
    let symlink_error = writer
        .create(&symlink_root, &initial_site_inputs())
        .expect_err("symlink");
    assert!(matches!(
        symlink_error,
        InitialSiteWriteError::Symlink { .. }
    ));
}

#[test]
fn shared_site_lock_excludes_initialization_and_lock_file_is_not_stale_state() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let shared = acquire_shared_site_lock(&root, Duration::ZERO).expect("shared backend lock");
    let lock_path = PathBuf::from(format!("{}.lock", root.display()));
    let contender = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("lock contender");

    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(shared);
    contender.try_lock().expect("released exclusive lock");
    drop(contender);

    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&root, &initial_site_inputs())
        .expect("unlocked lock file does not imply stale ownership");
}

#[test]
fn site_writers_refuse_a_running_site_without_waiting() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let inputs = initial_site_inputs();
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer.create(&root, &inputs).expect("initial site");
    let site_before = fs::read(root.join(SITE_FILE)).expect("site bytes");
    let request = InitialSiteRequest::new(
        [InitialInferenceRuntime::Simulated],
        [InitialApplicationRuntime::Simulated],
    );
    let _running = acquire_shared_site_lock(&root, Duration::ZERO).expect("running site lock");

    assert!(matches!(
        writer.initialize(&root, &inputs, &request, true),
        Err(InitialSiteWriteError::SiteRunning)
    ));
    assert!(matches!(
        writer.replace(&root, &inputs),
        Err(InitialSiteWriteError::SiteRunning)
    ));
    assert!(matches!(
        writer.create(&root, &inputs),
        Err(InitialSiteWriteError::SiteRunning)
    ));
    assert_eq!(
        fs::read(root.join(SITE_FILE)).expect("site bytes after"),
        site_before
    );
}

#[test]
fn shared_site_lock_waits_for_a_writer_then_reports_the_held_lock() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = super::try_acquire_exclusive_site_lock(&root)
        .expect("writer lock")
        .expect("uncontended writer lock");
    let wait = Duration::from_millis(100);

    let started = Instant::now();
    let error = acquire_shared_site_lock(&root, wait).expect_err("held writer lock");

    assert!(started.elapsed() >= wait);
    let SiteInputLockError::Held { path, waited } = error else {
        panic!("expected held lock, got {error:?}");
    };
    assert_eq!(path, PathBuf::from(format!("{}.lock", root.display())));
    assert_eq!(waited, wait);

    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        drop(writer);
    });
    acquire_shared_site_lock(&root, Duration::from_secs(30))
        .expect("shared lock after the writer finishes");
    release.join().expect("writer thread");
}

#[test]
fn rollback_restores_the_backup_after_a_swap_failure() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    let writer = InitialSiteInputWriter::new(SiteInputBundleValidator::for_test());
    writer
        .create(&root, &initial_site_inputs())
        .expect("initial site");
    let backup = backup_path(&root).expect("backup path");
    fs::rename(&root, &backup).expect("backup move");

    let error = writer
        .rollback_backup(
            &root,
            &backup,
            std::io::Error::other("simulated replacement failure"),
        )
        .expect_err("rollback reports original failure");

    assert!(matches!(error, InitialSiteWriteError::Storage(_)));
    assert!(root.join("site.yaml").is_file());
    assert!(!backup.exists());
}

#[test]
fn temporary_creation_write_and_file_sync_failures_leave_no_target() {
    for failure in [
        FailurePoint::CreateDirectory,
        FailurePoint::Write,
        FailurePoint::FileSync,
    ] {
        let fixture = SiteRoot::new();
        let root = fixture.root.join("site");

        let error = writer_with_failure(failure)
            .create(&root, &initial_site_inputs())
            .expect_err("staging failure");

        assert!(matches!(error, InitialSiteWriteError::Storage(_)));
        assert!(!root.exists());
        assert_no_staging_directory(&fixture.root);
    }
}

#[test]
fn staging_directory_sync_failure_leaves_no_target() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");

    let error = writer_with_failure(FailurePoint::StagingSync)
        .create(&root, &initial_site_inputs())
        .expect_err("staging sync failure");

    assert!(matches!(error, InitialSiteWriteError::Storage(_)));
    assert!(!root.exists());
    assert_no_staging_directory(&fixture.root);
}

#[test]
fn install_rename_and_parent_sync_failures_expose_safe_states() {
    let install_fixture = SiteRoot::new();
    let install_root = install_fixture.root.join("site");
    let install_error = writer_with_failure(FailurePoint::InstallRename)
        .create(&install_root, &initial_site_inputs())
        .expect_err("install rename failure");
    assert!(matches!(install_error, InitialSiteWriteError::Storage(_)));
    assert!(!install_root.exists());
    assert_no_staging_directory(&install_fixture.root);

    let sync_fixture = SiteRoot::new();
    let sync_root = sync_fixture.root.join("site");
    let sync_error = writer_with_failure(FailurePoint::ParentSync(1))
        .create(&sync_root, &initial_site_inputs())
        .expect_err("parent sync failure");
    assert!(matches!(
        sync_error,
        InitialSiteWriteError::OutcomeUncertain(_)
    ));
    assert_eq!(
        directory_entries(&sync_root),
        ["node.yaml", "site.yaml", "trust"]
    );
}

#[test]
fn backup_and_swap_failures_preserve_or_restore_the_old_target() {
    let backup_fixture = SiteRoot::new();
    let backup_root = backup_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&backup_root, &initial_site_inputs())
        .expect("initial site");
    let backup_error = writer_with_failure(FailurePoint::BackupRename)
        .replace(&backup_root, &initial_site_inputs())
        .expect_err("backup rename failure");
    assert!(matches!(backup_error, InitialSiteWriteError::Storage(_)));
    assert_eq!(
        directory_entries(&backup_root),
        ["node.yaml", "site.yaml", "trust"]
    );
    assert_no_staging_directory(&backup_fixture.root);

    let swap_fixture = SiteRoot::new();
    let swap_root = swap_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&swap_root, &initial_site_inputs())
        .expect("initial site");
    let swap_error = writer_with_failure(FailurePoint::InstallRename)
        .replace(&swap_root, &initial_site_inputs())
        .expect_err("swap rename failure");
    assert!(matches!(swap_error, InitialSiteWriteError::Storage(_)));
    assert_eq!(
        directory_entries(&swap_root),
        ["node.yaml", "site.yaml", "trust"]
    );
    assert_no_backup_directory(&swap_fixture.root);
}

#[test]
fn rollback_rename_failure_retains_recoverable_backup() {
    let fixture = SiteRoot::new();
    let root = fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&root, &initial_site_inputs())
        .expect("initial site");

    let error = writer_with_failure(FailurePoint::InstallRename)
        .replace(&root, &initial_site_inputs())
        .expect_err("replacement rename failure");

    assert!(matches!(error, InitialSiteWriteError::Storage(_)));
    assert!(root.exists());
    assert_no_backup_directory(&fixture.root);
}

#[test]
fn replacement_parent_sync_failures_retain_recovery_material() {
    let backup_fixture = SiteRoot::new();
    let backup_root = backup_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&backup_root, &initial_site_inputs())
        .expect("initial site");
    let backup_error = writer_with_failure(FailurePoint::ParentSync(1))
        .replace(&backup_root, &initial_site_inputs())
        .expect_err("backup parent sync failure");
    assert!(matches!(
        backup_error,
        InitialSiteWriteError::OutcomeUncertain(_)
    ));
    assert!(backup_root.exists());
    assert_no_backup_directory(&backup_fixture.root);

    let target_fixture = SiteRoot::new();
    let target_root = target_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&target_root, &initial_site_inputs())
        .expect("initial site");
    let target_error = writer_with_failure(FailurePoint::ParentSync(1))
        .replace(&target_root, &initial_site_inputs())
        .expect_err("target parent sync failure");
    assert!(matches!(
        target_error,
        InitialSiteWriteError::OutcomeUncertain(_)
    ));
    assert_eq!(
        directory_entries(&target_root),
        ["node.yaml", "site.yaml", "trust"]
    );
    assert_no_backup_directory(&target_fixture.root);
}

#[test]
fn rollback_and_backup_cleanup_directory_sync_failures_are_uncertain() {
    let rollback_fixture = SiteRoot::new();
    let rollback_root = rollback_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&rollback_root, &initial_site_inputs())
        .expect("initial site");
    let rollback_error = writer_with_failure(FailurePoint::ParentSync(1))
        .replace(&rollback_root, &initial_site_inputs())
        .expect_err("rollback directory sync failure");
    assert!(matches!(
        rollback_error,
        InitialSiteWriteError::OutcomeUncertain(_)
    ));
    assert_eq!(
        directory_entries(&rollback_root),
        ["node.yaml", "site.yaml", "trust"]
    );

    let cleanup_fixture = SiteRoot::new();
    let cleanup_root = cleanup_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&cleanup_root, &initial_site_inputs())
        .expect("initial site");
    let cleanup_error = writer_with_failure(FailurePoint::ParentSync(1))
        .replace(&cleanup_root, &initial_site_inputs())
        .expect_err("backup cleanup directory sync failure");
    assert!(matches!(
        cleanup_error,
        InitialSiteWriteError::OutcomeUncertain(_)
    ));
    assert_eq!(
        directory_entries(&cleanup_root),
        ["node.yaml", "site.yaml", "trust"]
    );
    assert_no_backup_directory(&cleanup_fixture.root);
}

#[test]
fn backup_cleanup_failure_is_recoverable_and_cross_filesystem_is_refused() {
    let cleanup_fixture = SiteRoot::new();
    let cleanup_root = cleanup_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&cleanup_root, &initial_site_inputs())
        .expect("initial site");
    let cleanup_error = writer_with_failure(FailurePoint::BackupRemoval)
        .replace(&cleanup_root, &initial_site_inputs())
        .expect_err("backup cleanup failure");
    assert!(matches!(
        cleanup_error,
        InitialSiteWriteError::RecoverableBackup { .. }
    ));
    assert_eq!(
        directory_entries(&cleanup_root),
        ["node.yaml", "site.yaml", "trust"]
    );
    assert_backup_directory(&cleanup_fixture.root);

    let cross_fixture = SiteRoot::new();
    let cross_root = cross_fixture.root.join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&cross_root, &initial_site_inputs())
        .expect("initial site");
    let cross_error = writer_with_failure(FailurePoint::CrossFilesystem)
        .replace(&cross_root, &initial_site_inputs())
        .expect_err("cross-filesystem replacement");
    assert!(matches!(
        cross_error,
        InitialSiteWriteError::CrossFilesystem
    ));
    assert_eq!(
        directory_entries(&cross_root),
        ["node.yaml", "site.yaml", "trust"]
    );
}

fn writer_with_failure(failure: FailurePoint) -> InitialSiteInputWriter {
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .with_filesystem(Arc::new(FailingFileSystem::new(failure)))
}

fn assert_no_staging_directory(root: &Path) {
    assert!(
        directory_entries(root)
            .iter()
            .all(|entry| !entry.starts_with(".site.init-stage-"))
    );
}

fn assert_backup_directory(root: &Path) {
    assert!(
        directory_entries(root)
            .iter()
            .any(|entry| entry.starts_with(".site.init-backup-"))
    );
}

fn assert_no_backup_directory(root: &Path) {
    assert!(
        directory_entries(root)
            .iter()
            .all(|entry| !entry.starts_with(".site.init-backup-"))
    );
}

#[derive(Clone, Copy)]
enum FailurePoint {
    CreateDirectory,

    Write,

    FileSync,

    StagingSync,

    BackupRename,

    InstallRename,

    ParentSync(usize),

    BackupRemoval,

    CrossFilesystem,
}

struct FailingFileSystem {
    failure: FailurePoint,
    inner: StdDirectoryTransactionFileSystem,
    parent_syncs: Mutex<usize>,
}

impl FailingFileSystem {
    fn new(failure: FailurePoint) -> Self {
        Self {
            failure,
            inner: StdDirectoryTransactionFileSystem,
            parent_syncs: Mutex::new(0),
        }
    }

    fn fail_rename(&self, from: &Path, to: &Path) -> bool {
        match self.failure {
            FailurePoint::BackupRename => is_backup_path(to),
            FailurePoint::InstallRename => is_staging_path(from),
            _ => false,
        }
    }
}

impl DirectoryTransactionFileSystem for FailingFileSystem {
    fn create_directory(&self, path: &Path) -> io::Result<()> {
        if matches!(self.failure, FailurePoint::CreateDirectory) {
            return Err(io::Error::other(
                "injected staging directory creation failure",
            ));
        }
        self.inner.create_directory(path)
    }

    fn create_file(&self, path: &Path) -> io::Result<fs::File> {
        self.inner.create_file(path)
    }

    fn write_all(&self, file: &mut fs::File, contents: &[u8]) -> io::Result<()> {
        if matches!(self.failure, FailurePoint::Write) {
            return Err(io::Error::other("injected staging write failure"));
        }
        self.inner.write_all(file, contents)
    }

    fn sync_file(&self, file: &fs::File) -> io::Result<()> {
        if matches!(self.failure, FailurePoint::FileSync) {
            return Err(io::Error::other("injected staging file sync failure"));
        }
        self.inner.sync_file(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_rename(from, to) {
            return Err(io::Error::other("injected rename failure"));
        }
        self.inner.rename(from, to)
    }

    fn remove_directory_all(&self, path: &Path) -> io::Result<()> {
        if matches!(self.failure, FailurePoint::BackupRemoval) && is_backup_path(path) {
            return Err(io::Error::other("injected backup cleanup failure"));
        }
        self.inner.remove_directory_all(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if matches!(self.failure, FailurePoint::BackupRemoval) && is_backup_path(path) {
            return Err(io::Error::other("injected backup cleanup failure"));
        }
        self.inner.remove_file(path)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        if matches!(self.failure, FailurePoint::StagingSync) && is_staging_path(path) {
            return Err(io::Error::other("injected staging directory sync failure"));
        }
        let expected_parent_sync = match self.failure {
            FailurePoint::ParentSync(expected) if !is_staging_path(path) => Some(expected),
            _ => None,
        };
        if let Some(expected) = expected_parent_sync {
            let mut parent_syncs = self.parent_syncs.lock().expect("parent sync count");
            *parent_syncs += 1;
            if *parent_syncs == expected {
                return Err(io::Error::other("injected parent directory sync failure"));
            }
        }
        self.inner.sync_directory(path)
    }

    fn same_filesystem(&self, target: &Path, parent: &Path) -> io::Result<bool> {
        if matches!(self.failure, FailurePoint::CrossFilesystem) {
            return Ok(false);
        }
        self.inner.same_filesystem(target, parent)
    }
}

fn is_staging_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .starts_with(".site.init-stage-")
    })
}

fn is_backup_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .starts_with(".site.init-backup-")
    })
}

fn trust_bytes(site_root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let trust_root = site_root.join("trust");
    let mut bytes = std::collections::BTreeMap::new();
    for name in ["node.der", "node-key.pk8", "site-ca.der", "site-ca-key.pk8"] {
        bytes.insert(
            name.to_owned(),
            fs::read(trust_root.join(name)).expect("trust bytes"),
        );
    }
    for entry in fs::read_dir(trust_root.join("worker-pins")).expect("worker pins") {
        let entry = entry.expect("worker pin");
        bytes.insert(
            format!("worker-pins/{}", entry.file_name().to_string_lossy()),
            fs::read(entry.path()).expect("worker pin bytes"),
        );
    }
    bytes
}

fn directory_entries(root: &std::path::Path) -> Vec<String> {
    let mut entries = fs::read_dir(root)
        .expect("Site root")
        .map(|entry| {
            entry
                .expect("Site entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

struct SiteRoot {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl SiteRoot {
    fn new() -> Self {
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("fixture root");
        let root = directory.path().to_owned();
        Self {
            _directory: directory,
            root,
        }
    }
}
