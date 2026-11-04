use std::{
    error::Error as _,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use dotcore_runtime_backends::native::test_support::NativeTestHost;

use super::{
    SiteDeleteTransaction, SitePersistenceCleanup, SiteResetIdentityError, SiteResetLock,
    SiteResetLockError, SiteResetPublisher, SiteResetSession, SiteResetTransaction,
    SiteResetTransactionError, SiteRuntimeCleanup,
};
use crate::node::folder::{WorkerNodeCustody, WorkerNodeCustodyError};
use crate::{
    initial::{
        input_store::{InitialSiteInputWriter, InitialSiteWriteError, acquire_shared_site_lock},
        inputs::InitialSiteInputs,
        validation::SiteInputBundleValidator,
    },
    test_support::initial_site_inputs,
};

const HELPER_SITE_ROOT: &str = "DOTCORE_TEST_SHARED_SITE_ROOT";
const HELPER_READY_PATH: &str = "DOTCORE_TEST_SHARED_SITE_READY";
static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn running_site_is_typed_and_the_failed_attempt_mutates_nothing() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("marker"), "unchanged").expect("Site marker");
    fs::write(fixture.lock_path(), "holder metadata").expect("lock metadata");
    let shared =
        acquire_shared_site_lock(&fixture.site_root, Duration::ZERO).expect("shared backend lock");
    let before = fixture.snapshot();

    let error =
        SiteResetLock::try_acquire(&fixture.site_root).expect_err("running site must reject reset");

    assert!(matches!(
        error,
        SiteResetLockError::SiteRunning { site_root }
            if site_root == fixture.site_root
    ));
    assert_eq!(fixture.snapshot(), before);
    drop(shared);
}

#[test]
fn preacquired_session_retains_exclusion_after_identity_observation() {
    let fixture = SiteLockFixture::new();
    fs::write(
        fixture.site_root.join("site.yaml"),
        "siteId: retained-site\n",
    )
    .expect("retained site identity");

    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("begin reset session");
    let competing = SiteResetLock::try_acquire(&fixture.site_root)
        .expect_err("session must retain exclusive reset lock");

    assert_eq!(session.site_root(), fixture.site_root);
    assert!(matches!(competing, SiteResetLockError::SiteRunning { .. }));
    drop(session);
    SiteResetLock::try_acquire(&fixture.site_root).expect("session drop releases lock");
}

#[test]
fn existing_runtime_bindings_select_kinds_fall_back_when_damaged_and_name_unknown_adapters() {
    let docker = dotcore_runtime::RuntimeBindingKind::Docker;
    let native = dotcore_runtime::RuntimeBindingKind::Native;
    let simulated = dotcore_runtime::RuntimeBindingKind::Simulated;
    for (site_yaml, expected) in [
        ("siteId: retained-site\n", Ok(Some(vec![]))),
        (
            "siteId: retained-site\nexecutionTargets: []\n",
            Ok(Some(vec![])),
        ),
        (
            "siteId: retained-site\nexecutionTargets:\n- modelRuntime: { simulatedRuntime: {} }\n- applicationRuntime: { simulatedRuntime: {} }\n",
            Ok(Some(vec![simulated])),
        ),
        (
            "siteId: retained-site\nexecutionTargets:\n- modelRuntime: { simulatedRuntime: {} }\n- applicationRuntime: { dockerRuntime: {} }\n",
            Ok(Some(vec![docker, simulated])),
        ),
        (
            "siteId: retained-site\nexecutionTargets:\n- modelRuntime: { nativeRuntime: { workloadLifetime: NATIVE_WORKLOAD_LIFETIME_EPHEMERAL } }\n",
            Ok(Some(vec![native])),
        ),
        (
            "siteId: retained-site\nexecutionTargets:\n- executionTargetId: unbound\n",
            Ok(None),
        ),
        (
            "siteId: retained-site\nexecutionTargets: broken\n",
            Ok(None),
        ),
        (
            "siteId: retained-site\nexecutionTargets:\n- executionTargetId: unbound\n- modelRuntime: { futureRuntime: {} }\n",
            Err("futureRuntime".to_owned()),
        ),
    ] {
        let fixture = SiteLockFixture::new();
        fs::write(fixture.site_root.join("site.yaml"), site_yaml).expect("site YAML");

        let identity = super::existing_site_identity(&fixture.site_root)
            .expect("readable identity")
            .expect("existing site");

        assert_eq!(identity.site_id(), "retained-site");
        assert_eq!(
            identity
                .runtime_binding_kinds()
                .map(|kinds| kinds.map(|kinds| kinds.into_iter().collect::<Vec<_>>())),
            expected,
            "{site_yaml}"
        );
    }
}

#[test]
fn an_unknown_runtime_binding_refuses_cleanup_selection_instead_of_falling_back() {
    let fixture = SiteLockFixture::new();
    fs::write(
        fixture.site_root.join("site.yaml"),
        "siteId: retained-site\nexecutionTargets:\n- modelRuntime: { futureRuntime: {} }\n",
    )
    .expect("site YAML");
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    let error = match session.runtime_cleanup() {
        Ok(_) => panic!("unknown binding selected a cleanup"),
        Err(error) => error,
    };

    assert!(matches!(
        &error,
        dotcore_runtime::SiteRuntimeResetConfigurationError::UnknownBinding { adapter }
            if adapter == "futureRuntime"
    ));
    assert_eq!(
        error.to_string(),
        "an execution target binds unknown runtime adapter \"futureRuntime\", whose resources site cleanup cannot classify"
    );
}

#[test]
fn dropped_lock_and_stale_metadata_do_not_block_reset() {
    let fixture = SiteLockFixture::new();
    let lock_path = fixture.lock_path();
    fs::write(&lock_path, "stale pid=424242").expect("stale lock metadata");
    let shared =
        acquire_shared_site_lock(&fixture.site_root, Duration::ZERO).expect("shared backend lock");
    drop(shared);

    let reset =
        SiteResetLock::try_acquire(&fixture.site_root).expect("dropped shared lock admits reset");

    assert_eq!(
        fs::read_to_string(&lock_path).expect("preserved lock metadata"),
        "stale pid=424242"
    );
    assert!(lock_path.is_file());
    drop(reset);
    assert!(lock_path.is_file());
}

#[test]
fn symlink_and_open_failures_are_storage_errors_not_running_sites() {
    use std::os::unix::fs::symlink;

    let symlink_fixture = SiteLockFixture::new();
    let target = symlink_fixture.root.join("outside-lock");
    fs::write(&target, "outside").expect("outside lock target");
    symlink(&target, symlink_fixture.lock_path()).expect("lock symlink");

    let symlink_error =
        SiteResetLock::try_acquire(&symlink_fixture.site_root).expect_err("symlink lock refusal");

    assert!(matches!(symlink_error, SiteResetLockError::Storage(_)));
    assert!(symlink_error.source().is_some());
    assert_eq!(
        fs::read_to_string(target).expect("outside target"),
        "outside"
    );

    let open_fixture = SiteLockFixture::new();
    fs::create_dir(open_fixture.lock_path()).expect("directory at lock path");

    let open_error =
        SiteResetLock::try_acquire(&open_fixture.site_root).expect_err("lock open failure");

    assert!(matches!(open_error, SiteResetLockError::Storage(_)));
    assert!(open_error.source().is_some());
}

#[test]
fn permission_and_non_busy_lock_errors_remain_storage_failures() {
    for (kind, message) in [
        (std::io::ErrorKind::PermissionDenied, "permission denied"),
        (std::io::ErrorKind::Other, "injected lock failure"),
    ] {
        let error =
            super::reset_lock_error(crate::initial::input_store::SiteInputLockError::Storage(
                anyhow::Error::new(std::io::Error::new(kind, message)),
            ));

        assert!(matches!(error, SiteResetLockError::Storage(_)));
        assert_eq!(error.source().expect("storage source").to_string(), message);
    }
}

#[test]
fn process_shared_locks_coexist_and_crash_releases_exclusive_reset() {
    let fixture = SiteLockFixture::new();
    let ready = fixture.root.join("child-ready");
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "reset::transaction::tests::shared_site_lock_process_helper",
            "--nocapture",
        ])
        .env(HELPER_SITE_ROOT, &fixture.site_root)
        .env(HELPER_READY_PATH, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("shared-lock helper process");
    wait_for_file(&ready, &mut child);

    let second_shared = acquire_shared_site_lock(&fixture.site_root, Duration::ZERO)
        .expect("second shared backend lock");
    assert!(matches!(
        SiteResetLock::try_acquire(&fixture.site_root),
        Err(SiteResetLockError::SiteRunning { .. })
    ));
    drop(second_shared);

    child.kill().expect("terminate shared-lock helper");
    let status = child.wait().expect("reap shared-lock helper");
    assert!(!status.success());

    let reset = SiteResetLock::try_acquire(&fixture.site_root)
        .expect("process termination releases shared lock");
    assert!(fixture.lock_path().is_file());
    drop(reset);
}

#[tokio::test]
async fn absent_site_skips_runtime_cleanup_and_publishes_exact_fresh_inputs() {
    let fixture = SiteLockFixture::new_without_site();
    let state = Arc::new(Mutex::new(ResetState::default()));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
    let inputs = initial_site_inputs();

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect("absent site reset");

    assert_eq!(
        state.lock().expect("reset state").calls,
        ["persistence", "publish"]
    );
    assert_eq!(
        fs::read_to_string(fixture.site_root.join("site.yaml")).expect("fresh site YAML"),
        inputs.site_yaml().expect("expected site YAML")
    );
    assert_eq!(
        fs::read_to_string(fixture.site_root.join("node.yaml")).expect("fresh node YAML"),
        inputs.node_yaml().expect("expected node YAML")
    );
    assert_fresh_site_tree(&fixture, &inputs);
}

#[tokio::test]
async fn reset_rotates_manager_trust_and_clears_worker_pins() {
    let fixture = SiteLockFixture::new_without_site();
    let inputs = initial_site_inputs();
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .create(&fixture.site_root, &inputs)
        .expect("initialize manager trust");
    let trust = fixture.site_root.join("trust");
    let old_anchor = fs::read(trust.join("site-ca.der")).expect("initial trust anchor");
    fs::write(
        trust.join("worker-pins/old-worker.sha256"),
        b"retired-pin\n",
    )
    .expect("existing worker pin");
    let transaction = transaction(
        Arc::new(Mutex::new(ResetState::default())),
        RuntimeBehavior::Success,
        false,
    );

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect("reset site");

    assert_ne!(
        fs::read(trust.join("site-ca.der")).expect("rotated trust anchor"),
        old_anchor
    );
    assert_eq!(
        fs::read_dir(trust.join("worker-pins"))
            .expect("new pin registry")
            .count(),
        0
    );
    assert_fresh_site_tree(&fixture, &inputs);
}

#[tokio::test]
async fn reset_replaces_a_trustless_older_site() {
    let fixture = existing_site_fixture("older-site");
    let inputs = initial_site_inputs();
    let transaction = transaction(
        Arc::new(Mutex::new(ResetState::default())),
        RuntimeBehavior::Success,
        false,
    );

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect("reset older site");

    assert_fresh_site_tree(&fixture, &inputs);
}

#[tokio::test]
async fn existing_site_uses_only_valid_identity_despite_broken_declarations_and_state() {
    let fixture = SiteLockFixture::new();
    fs::write(
        fixture.site_root.join("site.yaml"),
        "siteId: retained-site\nmodelDeployments:\n- invalidDeclaration: true\n",
    )
    .expect("broken desired state");
    fs::write(fixture.site_root.join("state.pb"), [0xff, 0x00, 0x80])
        .expect("broken current state");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["owned-runtime".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
    let inputs = initial_site_inputs();

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect("broken retained state does not block reset");

    assert_eq!(
        state.lock().expect("reset state").calls,
        ["runtime:retained-site", "persistence", "publish"]
    );
    assert_fresh_site_tree(&fixture, &inputs);
}

#[tokio::test]
async fn reset_and_delete_remove_the_sites_workload_caches_and_keep_other_sites() {
    for deleting in [false, true] {
        let fixture = existing_site_fixture("cached-site");
        let cache = tempfile::tempdir().expect("cache root");
        let cleanups = crate::test_support::site_runtime_cleanups_with_cache_root(cache.path());
        let layout = dotcore_domain::CacheLayout(cache.path());
        let entry = |site: &str, version: &str| {
            cache
                .path()
                .join("workload-cache")
                .join(version)
                .join(site)
                .join("engine-artifact/key/engine-cache/entry")
        };
        for site in ["cached-site", "other-site"] {
            for version in ["v2", "v1"] {
                let path = entry(site, version);
                fs::create_dir_all(path.parent().expect("parent")).expect("cache folder");
                fs::write(&path, site).expect("cache entry");
            }
        }
        let [current, earlier] = layout.site_workload_caches("cached-site");
        assert!(current.is_dir() && earlier.is_dir());
        let state = Arc::new(Mutex::new(ResetState::default()));

        if deleting {
            let session = SiteResetSession::begin(&fixture.site_root, cleanups).expect("session");
            delete_transaction(state, RuntimeBehavior::Success)
                .delete(session)
                .await
                .expect("delete");
        } else {
            transaction(state, RuntimeBehavior::Success, false)
                .reset(&fixture.site_root, cleanups, &initial_site_inputs())
                .await
                .expect("reset");
        }

        assert!(!current.exists(), "deleting={deleting}");
        assert!(!earlier.exists(), "deleting={deleting}");
        for version in ["v2", "v1"] {
            assert_eq!(
                fs::read_to_string(entry("other-site", version)).expect("other site kept"),
                "other-site"
            );
        }
    }
}

#[tokio::test]
async fn failed_runtime_cleanup_keeps_the_sites_workload_caches() {
    let fixture = existing_site_fixture("cached-site");
    let cache = tempfile::tempdir().expect("cache root");
    let entry = cache
        .path()
        .join("workload-cache/v2/cached-site/engine-artifact/key/engine-cache/entry");
    fs::create_dir_all(entry.parent().expect("parent")).expect("cache folder");
    fs::write(&entry, "warm").expect("cache entry");
    let transaction = transaction(
        Arc::new(Mutex::new(ResetState::default())),
        RuntimeBehavior::FailWithoutMutation,
        false,
    );

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups_with_cache_root(cache.path()),
            &initial_site_inputs(),
        )
        .await
        .expect_err("runtime cleanup failure");

    assert_eq!(fs::read_to_string(entry).expect("cache kept"), "warm");
}

#[tokio::test]
async fn invalid_existing_identity_causes_zero_cleanup_or_publication_mutation() {
    for site_yaml in ["nodes: []\n", "siteId: Invalid\n", "siteId: [\n"] {
        let fixture = SiteLockFixture::new();
        fs::write(fixture.site_root.join("site.yaml"), site_yaml).expect("invalid site YAML");
        fs::write(fixture.site_root.join("state.pb"), "preserved state").expect("retained state");
        let before = fixture.site_tree();
        let state = Arc::new(Mutex::new(ResetState {
            runtime_resources: vec!["owned-runtime".to_owned()],
            ..ResetState::default()
        }));
        let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);

        let error = transaction
            .reset(
                &fixture.site_root,
                crate::test_support::site_runtime_cleanups(),
                &initial_site_inputs(),
            )
            .await
            .expect_err("invalid identity must fail");

        assert!(matches!(
            error,
            SiteResetTransactionError::ExistingSiteIdentity(_)
        ));
        assert_eq!(
            state.lock().expect("reset state").clone(),
            ResetState {
                runtime_resources: vec!["owned-runtime".to_owned()],
                ..ResetState::default()
            }
        );
        assert_eq!(fixture.site_tree(), before);
    }

    for site_yaml_is_directory in [false, true] {
        let fixture = SiteLockFixture::new();
        if site_yaml_is_directory {
            fs::create_dir(fixture.site_root.join("site.yaml"))
                .expect("unreadable site YAML entry");
        }
        fs::write(fixture.site_root.join("state.pb"), "preserved state").expect("retained state");
        let before = fixture.site_tree();
        let state = Arc::new(Mutex::new(ResetState {
            runtime_resources: vec!["owned-runtime".to_owned()],
            ..ResetState::default()
        }));
        let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);

        let error = transaction
            .reset(
                &fixture.site_root,
                crate::test_support::site_runtime_cleanups(),
                &initial_site_inputs(),
            )
            .await
            .expect_err("missing or unreadable site YAML must fail");

        assert!(matches!(
            error,
            SiteResetTransactionError::ExistingSiteIdentity(_)
        ));
        assert_eq!(
            state.lock().expect("reset state").clone(),
            ResetState {
                runtime_resources: vec!["owned-runtime".to_owned()],
                ..ResetState::default()
            }
        );
        assert_eq!(fixture.site_tree(), before);
    }
}

#[tokio::test]
async fn fresh_simulated_inputs_do_not_limit_cleanup_of_retained_docker_residue() {
    let fixture = existing_site_fixture("retained-docker-site");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned(), "docker-application".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
    let inputs = initial_site_inputs();
    let fresh_yaml = inputs.site_yaml().expect("fresh site YAML");
    assert!(fresh_yaml.contains("simulatedRuntime"));
    assert!(!fresh_yaml.contains("dockerRuntime"));

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect("reset to fresh simulated inputs");

    assert_eq!(
        state.lock().expect("reset state").clone(),
        ResetState {
            calls: vec![
                "runtime:retained-docker-site".to_owned(),
                "persistence".to_owned(),
                "publish".to_owned(),
            ],
            runtime_resources: Vec::new(),
        }
    );
}

#[tokio::test]
async fn runtime_and_persistence_failures_preserve_exact_ordered_state_deltas() {
    let runtime_fixture = existing_site_fixture("runtime-failure-site");
    let runtime_before = runtime_fixture.site_tree();
    let runtime_state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["runtime-a".to_owned(), "runtime-b".to_owned()],
        ..ResetState::default()
    }));
    let runtime_transaction = transaction(
        Arc::clone(&runtime_state),
        RuntimeBehavior::FailWithoutMutation,
        false,
    );

    let runtime_error = runtime_transaction
        .reset(
            &runtime_fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &initial_site_inputs(),
        )
        .await
        .expect_err("Runtime cleanup failure");

    assert!(matches!(
        runtime_error,
        SiteResetTransactionError::RuntimeCleanup(_)
    ));
    assert_eq!(runtime_fixture.site_tree(), runtime_before);
    assert_eq!(
        runtime_state.lock().expect("runtime state").clone(),
        ResetState {
            calls: vec!["runtime:runtime-failure-site".to_owned()],
            runtime_resources: vec!["runtime-a".to_owned(), "runtime-b".to_owned()],
        }
    );

    let persistence_fixture = existing_site_fixture("persistence-failure-site");
    let persistence_before = persistence_fixture.site_tree();
    let persistence_state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["runtime-a".to_owned()],
        ..ResetState::default()
    }));
    let persistence_transaction = transaction(
        Arc::clone(&persistence_state),
        RuntimeBehavior::Success,
        true,
    );

    let persistence_error = persistence_transaction
        .reset(
            &persistence_fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &initial_site_inputs(),
        )
        .await
        .expect_err("persistence cleanup failure");

    assert!(matches!(
        persistence_error,
        SiteResetTransactionError::PersistenceCleanup(_)
    ));
    assert_eq!(persistence_fixture.site_tree(), persistence_before);
    assert_eq!(
        persistence_state.lock().expect("persistence state").clone(),
        ResetState {
            calls: vec![
                "runtime:persistence-failure-site".to_owned(),
                "persistence".to_owned(),
            ],
            runtime_resources: Vec::new(),
        }
    );
}

#[tokio::test]
async fn partial_runtime_failure_retries_to_exact_clean_state_before_publish() {
    let fixture = existing_site_fixture("retry-site");
    let retained_tree = fixture.site_tree();
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["runtime-a".to_owned(), "runtime-b".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(
        Arc::clone(&state),
        RuntimeBehavior::RemoveOneThenFailOnce,
        false,
    );
    let inputs = initial_site_inputs();

    let first_error = transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect_err("partial runtime cleanup");

    assert!(matches!(
        first_error,
        SiteResetTransactionError::RuntimeCleanup(_)
    ));
    assert_eq!(fixture.site_tree(), retained_tree);
    assert_eq!(
        state.lock().expect("partial state").clone(),
        ResetState {
            calls: vec!["runtime:retry-site".to_owned()],
            runtime_resources: vec!["runtime-b".to_owned()],
        }
    );

    transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &inputs,
        )
        .await
        .expect("idempotent runtime cleanup retry");

    assert_eq!(
        state.lock().expect("completed state").clone(),
        ResetState {
            calls: vec![
                "runtime:retry-site".to_owned(),
                "runtime:retry-site".to_owned(),
                "persistence".to_owned(),
                "publish".to_owned(),
            ],
            runtime_resources: Vec::new(),
        }
    );
    assert_fresh_site_tree(&fixture, &inputs);
}

#[tokio::test]
async fn uncertain_publication_is_reported_with_its_exact_visible_state() {
    let fixture = existing_site_fixture("uncertain-site");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["runtime-a".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false)
        .with_publisher(Box::new(UncertainPublisher {
            state: Arc::clone(&state),
        }));

    let error = transaction
        .reset(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
            &initial_site_inputs(),
        )
        .await
        .expect_err("uncertain fresh publication");

    assert!(matches!(
        error,
        SiteResetTransactionError::FreshInitialization(InitialSiteWriteError::OutcomeUncertain(_))
    ));
    assert_eq!(
        state.lock().expect("reset state").clone(),
        ResetState {
            calls: vec![
                "runtime:uncertain-site".to_owned(),
                "persistence".to_owned(),
                "publish".to_owned(),
            ],
            runtime_resources: Vec::new(),
        }
    );
    assert_eq!(
        fixture.site_tree(),
        vec![(
            "publication-uncertain".to_owned(),
            Some(b"visible".to_vec())
        )]
    );
}

#[test]
#[ignore = "helper process launched by process_shared_locks_coexist_and_crash_releases_exclusive_reset"]
fn shared_site_lock_process_helper() {
    let site_root =
        PathBuf::from(std::env::var_os(HELPER_SITE_ROOT).expect("helper site root environment"));
    let ready =
        PathBuf::from(std::env::var_os(HELPER_READY_PATH).expect("helper ready-path environment"));
    let _shared =
        acquire_shared_site_lock(&site_root, Duration::ZERO).expect("helper shared site lock");
    fs::write(ready, "ready").expect("helper readiness marker");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn assert_fresh_site_tree(fixture: &SiteLockFixture, inputs: &InitialSiteInputs) {
    let tree = fixture.site_tree();
    assert_eq!(
        tree.iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        [
            "node.yaml",
            "site.yaml",
            "trust/",
            "trust/node-key.pk8",
            "trust/node.der",
            "trust/site-ca-key.pk8",
            "trust/site-ca.der",
            "trust/worker-pins/",
        ]
    );
    assert_eq!(
        tree[0].1,
        Some(inputs.node_yaml().expect("node YAML").into_bytes())
    );
    assert_eq!(
        tree[1].1,
        Some(inputs.site_yaml().expect("site YAML").into_bytes())
    );
    crate::manager_trust::ManagerTrust::load(&fixture.site_root, inputs.site())
        .expect("valid fresh manager credentials and empty worker-pin registry");
}

fn delete_transaction(
    state: Arc<Mutex<ResetState>>,
    runtime_behavior: RuntimeBehavior,
) -> SiteDeleteTransaction {
    SiteDeleteTransaction::new(
        Box::new(TestRuntimeCleanup {
            state: Arc::clone(&state),
            behavior: Mutex::new(runtime_behavior),
        }),
        Box::new(TestPersistenceCleanup {
            state,
            fails: false,
        }),
    )
}

#[tokio::test]
async fn delete_inventories_without_mutation_then_cleans_runtime_before_removing_the_root() {
    let fixture = existing_site_fixture("doomed-site");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned(), "docker-application".to_owned()],
        ..ResetState::default()
    }));
    let transaction = delete_transaction(Arc::clone(&state), RuntimeBehavior::Success);
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    let inventory = transaction.inventory(&session).await.expect("inventory");

    assert_eq!(
        inventory
            .resources
            .iter()
            .map(|resource| resource.description.as_str())
            .collect::<Vec<_>>(),
        ["docker-model", "docker-application"]
    );
    assert!(fixture.site_root.join("state.pb").exists());
    assert_eq!(
        state.lock().expect("reset state").calls,
        ["inventory:doomed-site"]
    );

    transaction.delete(session).await.expect("delete");

    assert!(!fixture.site_root.exists());
    let state = state.lock().expect("reset state");
    assert_eq!(
        state.calls,
        [
            "inventory:doomed-site",
            "runtime:doomed-site",
            "persistence"
        ]
    );
    assert!(state.runtime_resources.is_empty());
}

#[tokio::test]
async fn delete_keeps_the_site_root_when_runtime_cleanup_fails() {
    let fixture = existing_site_fixture("doomed-site");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned()],
        ..ResetState::default()
    }));
    let transaction = delete_transaction(Arc::clone(&state), RuntimeBehavior::FailWithoutMutation);
    let before = fixture.site_tree();
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    let error = transaction
        .delete(session)
        .await
        .expect_err("runtime failure");

    assert!(matches!(
        error,
        SiteResetTransactionError::RuntimeCleanup(_)
    ));
    assert_eq!(fixture.site_tree(), before);
    assert_eq!(
        state.lock().expect("reset state").calls,
        ["runtime:doomed-site"]
    );
}

#[tokio::test]
async fn unidentified_delete_removes_an_invalid_site_without_contacting_runtime() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("site.yaml"), "siteId: [\n").expect("invalid site YAML");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned()],
        ..ResetState::default()
    }));
    let transaction = delete_transaction(Arc::clone(&state), RuntimeBehavior::Success);
    assert!(
        SiteResetSession::begin(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups()
        )
        .is_err()
    );
    let session = SiteResetSession::begin_unidentified(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    assert!(
        transaction
            .inventory(&session)
            .await
            .expect("inventory")
            .resources
            .is_empty()
    );
    transaction.delete(session).await.expect("delete");

    assert!(!fixture.site_root.exists());
    let state = state.lock().expect("reset state");
    assert_eq!(state.calls, ["persistence"]);
    assert_eq!(state.runtime_resources, ["docker-model"]);
}

#[test]
fn unidentified_session_refuses_an_unrecognizable_directory() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("notes.txt"), "not a site").expect("unrelated file");

    let error = SiteResetSession::begin_unidentified(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect_err("unrelated directory");

    assert!(matches!(
        error,
        SiteResetTransactionError::ExistingSiteIdentity(
            SiteResetIdentityError::UnrecognizedRoot { .. }
        )
    ));
    assert!(fixture.site_root.join("notes.txt").is_file());
}

#[test]
fn unidentified_site_session_refuses_a_node_folder() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("worker.yaml"), "node folder").expect("node folder");
    fs::write(fixture.site_root.join("node.yaml"), "worker node").expect("worker node");

    let error = SiteResetSession::begin_unidentified_site(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect_err("node folder");

    assert!(matches!(
        error,
        SiteResetTransactionError::ExistingSiteIdentity(
            SiteResetIdentityError::UnrecognizedRoot { .. }
        )
    ));
    assert!(fixture.site_root.join("worker.yaml").is_file());
    drop(
        SiteResetSession::begin_unidentified(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups(),
        )
        .expect("delete still accepts a node folder"),
    );
}

#[test]
fn provisioned_workers_counts_worker_pins() {
    let fixture = existing_site_fixture("pinned-site");
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");
    assert_eq!(session.provisioned_workers().expect("no pins"), 0);

    let pins = fixture.site_root.join("trust/worker-pins");
    fs::create_dir_all(&pins).expect("pin registry");
    fs::write(pins.join("worker-one.sha256"), "pin").expect("pin one");
    fs::write(pins.join("worker-two.sha256"), "pin").expect("pin two");
    fs::write(pins.join("notes.txt"), "not a pin").expect("unrelated file");

    assert_eq!(session.provisioned_workers().expect("pins"), 2);
}

#[test]
fn delete_refuses_while_the_site_lock_is_held() {
    let fixture = SiteLockFixture::new();
    let _held = SiteResetSession::begin_unidentified(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("first");

    assert!(matches!(
        SiteResetSession::begin_unidentified(
            &fixture.site_root,
            crate::test_support::site_runtime_cleanups()
        ),
        Err(SiteResetTransactionError::Lock(_))
    ));
}

#[test]
fn a_running_node_blocks_both_session_entry_points_and_is_released_on_drop() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("worker.yaml"), "worker").expect("worker manifest");
    fs::write(fixture.site_root.join("node-store"), "store").expect("node store");
    let custody = WorkerNodeCustody::try_acquire(&fixture.site_root).expect("node custody");
    let before = fixture.site_tree();

    let unidentified = SiteResetSession::begin_unidentified(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect_err("running node blocks unidentified session");
    let identified = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect_err("running node blocks identified session");

    for error in [unidentified, identified] {
        assert!(
            matches!(
                &error,
                SiteResetTransactionError::Lock(SiteResetLockError::NodeRunning { site_root })
                    if site_root == &fixture.site_root
            ),
            "{error:?}"
        );
    }
    assert_eq!(fixture.site_tree(), before);
    // The sibling lock is released when the custody check fails.
    drop(SiteResetLock::try_acquire(&fixture.site_root).expect("sibling lock released"));

    drop(custody);
    let session = SiteResetSession::begin_unidentified(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("stopped node folder is accepted");
    assert!(matches!(
        WorkerNodeCustody::try_acquire(&fixture.site_root),
        Err(WorkerNodeCustodyError::AlreadyHeld)
    ));
    drop(session);
    drop(WorkerNodeCustody::try_acquire(&fixture.site_root).expect("custody released"));
}

#[test]
fn manager_empty_and_symlinked_manifest_folders_take_no_custody() {
    let lock_file = |root: &Path| root.join(crate::node::folder::WORKER_CUSTODY_LOCK_FILE);

    let manager = SiteLockFixture::new();
    fs::write(manager.site_root.join("site.yaml"), "siteId: site-a\n").expect("site.yaml");
    drop(
        SiteResetSession::begin_unidentified(
            &manager.site_root,
            crate::test_support::site_runtime_cleanups(),
        )
        .expect("manager session"),
    );
    assert!(!lock_file(&manager.site_root).exists());

    let empty = SiteLockFixture::new();
    drop(
        SiteResetSession::begin_unidentified(
            &empty.site_root,
            crate::test_support::site_runtime_cleanups(),
        )
        .expect("empty session"),
    );
    assert!(!lock_file(&empty.site_root).exists());

    let missing = SiteLockFixture::new_without_site();
    drop(
        SiteResetSession::begin_unidentified(
            &missing.site_root,
            crate::test_support::site_runtime_cleanups(),
        )
        .expect("missing session"),
    );
    assert!(!missing.site_root.exists());

    let linked = SiteLockFixture::new();
    let target = linked.root.join("elsewhere.yaml");
    fs::write(&target, "worker").expect("link target");
    std::os::unix::fs::symlink(&target, linked.site_root.join("worker.yaml"))
        .expect("manifest link");
    // Custody held on the folder would block a followed link; a symlink is not a node folder.
    let _held = WorkerNodeCustody::try_acquire(&linked.site_root).expect("custody");
    let error = SiteResetSession::begin_unidentified(
        &linked.site_root,
        crate::test_support::site_runtime_cleanups(),
    );
    assert!(error.is_ok(), "{error:?}");
    assert_eq!(fs::read(&target).expect("target"), b"worker");
}

#[tokio::test]
async fn unidentified_reset_replaces_an_invalid_site_without_contacting_runtime() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("site.yaml"), "siteId: [\n").expect("invalid site YAML");
    fs::write(fixture.site_root.join("state.pb"), "retained state").expect("retained state");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
    let inputs = initial_site_inputs();
    let session = SiteResetSession::begin_unidentified_site(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    transaction
        .reset_locked(session, &inputs)
        .await
        .expect("unidentified reset");

    let state = state.lock().expect("reset state");
    assert_eq!(state.calls, ["persistence", "publish"]);
    assert_eq!(state.runtime_resources, ["docker-model"]);
    assert_fresh_site_tree(&fixture, &inputs);
}

#[tokio::test]
async fn reset_inventory_lists_identified_resources_without_mutation() {
    let fixture = existing_site_fixture("retained-site");
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned(), "docker-application".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
    let before = fixture.site_tree();
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    let inventory = transaction.inventory(&session).await.expect("inventory");

    assert_eq!(
        inventory
            .resources
            .iter()
            .map(|resource| resource.description.as_str())
            .collect::<Vec<_>>(),
        ["docker-model", "docker-application"]
    );
    assert_eq!(fixture.site_tree(), before);
    let state = state.lock().expect("reset state");
    assert_eq!(state.calls, ["inventory:retained-site"]);
    assert_eq!(state.runtime_resources.len(), 2);
}

#[tokio::test]
async fn reset_inventory_of_an_absent_site_is_empty_without_contacting_runtime() {
    let fixture = SiteLockFixture::new_without_site();
    let state = Arc::new(Mutex::new(ResetState {
        runtime_resources: vec!["docker-model".to_owned()],
        ..ResetState::default()
    }));
    let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    let inventory = transaction.inventory(&session).await.expect("inventory");

    assert!(inventory.resources.is_empty());
    assert!(state.lock().expect("reset state").calls.is_empty());
}

#[tokio::test]
async fn every_removal_path_stops_native_workloads_before_removing_the_root() {
    /// How one removal path acquires its session and removes the root.
    #[derive(Clone, Copy, Debug)]
    enum RemovalPath {
        IdentifiedDelete,
        UnidentifiedDelete,
        WorkerFolderDelete,
        IdentifiedReset,
        UnidentifiedSiteReset,
    }

    for (path, files, expected_calls) in [
        (
            RemovalPath::IdentifiedDelete,
            &[
                ("site.yaml", "siteId: site-one\n"),
                ("node.yaml", "nodeId: node-a\n"),
            ][..],
            &["inventory:site-one", "runtime:site-one", "persistence"][..],
        ),
        (
            RemovalPath::UnidentifiedDelete,
            &[("site.yaml", "siteId: [\n")],
            &["persistence"],
        ),
        (
            RemovalPath::WorkerFolderDelete,
            &[("node.yaml", "nodeId: node-a\n"), ("worker.yaml", "worker")],
            &["persistence"],
        ),
        (
            RemovalPath::IdentifiedReset,
            &[
                ("site.yaml", "siteId: site-one\n"),
                ("node.yaml", "nodeId: node-a\n"),
            ],
            &[
                "inventory:site-one",
                "runtime:site-one",
                "persistence",
                "publish",
            ],
        ),
        (
            RemovalPath::UnidentifiedSiteReset,
            &[("site.yaml", "siteId: [\n")],
            &["persistence", "publish"],
        ),
    ] {
        let fixture = SiteLockFixture::new();
        for (name, contents) in files {
            fs::write(fixture.site_root.join(name), contents).expect("Site input");
        }
        let host = NativeTestHost::new(&fixture.site_root, "site-one");
        let workload = host.spawn("deployment-one").await;
        let state = Arc::new(Mutex::new(ResetState::default()));
        let session = match path {
            RemovalPath::IdentifiedDelete | RemovalPath::IdentifiedReset => {
                SiteResetSession::begin(
                    &fixture.site_root,
                    crate::test_support::site_runtime_cleanups(),
                )
            }
            RemovalPath::UnidentifiedDelete | RemovalPath::WorkerFolderDelete => {
                SiteResetSession::begin_unidentified(
                    &fixture.site_root,
                    crate::test_support::site_runtime_cleanups(),
                )
            }
            RemovalPath::UnidentifiedSiteReset => SiteResetSession::begin_unidentified_site(
                &fixture.site_root,
                crate::test_support::site_runtime_cleanups(),
            ),
        }
        .expect("session");

        let inventory = match path {
            RemovalPath::IdentifiedDelete
            | RemovalPath::UnidentifiedDelete
            | RemovalPath::WorkerFolderDelete => {
                let transaction = delete_transaction(Arc::clone(&state), RuntimeBehavior::Success);
                let inventory = transaction.inventory(&session).await.expect("inventory");
                transaction.delete(session).await.expect("delete");
                assert!(!fixture.site_root.exists(), "{path:?}");
                inventory
            }
            RemovalPath::IdentifiedReset | RemovalPath::UnidentifiedSiteReset => {
                let transaction = transaction(Arc::clone(&state), RuntimeBehavior::Success, false);
                let inputs = initial_site_inputs();
                let inventory = transaction.inventory(&session).await.expect("inventory");
                transaction
                    .reset_locked(session, &inputs)
                    .await
                    .expect("reset");
                assert_fresh_site_tree(&fixture, &inputs);
                inventory
            }
        };

        assert_eq!(
            inventory
                .resources
                .iter()
                .map(|resource| (resource.runtime, resource.description.as_str()))
                .collect::<Vec<_>>(),
            [(
                "Native runtime",
                format!(
                    "workload {} on execution target mac-gpu (running)",
                    workload.id
                )
                .as_str()
            )],
            "{path:?}"
        );
        assert!(!workload.engine_runs(), "{path:?}");
        assert_eq!(
            state.lock().expect("reset state").calls,
            expected_calls,
            "{path:?}"
        );
    }
}

#[tokio::test]
async fn untrusted_native_evidence_keeps_the_site_and_skips_every_runtime_cleanup() {
    let fixture = SiteLockFixture::new();
    fs::write(fixture.site_root.join("site.yaml"), "siteId: site-two\n").expect("site YAML");
    let host = NativeTestHost::new(&fixture.site_root, "site-one");
    let workload = host.spawn("deployment-one").await;
    let state = Arc::new(Mutex::new(ResetState::default()));
    let transaction = delete_transaction(Arc::clone(&state), RuntimeBehavior::Success);
    let session = SiteResetSession::begin(
        &fixture.site_root,
        crate::test_support::site_runtime_cleanups(),
    )
    .expect("session");

    let inventory = transaction.inventory(&session).await;
    let deleted = transaction.delete(session).await;

    for error in [
        inventory.expect_err("inventory"),
        deleted.expect_err("delete"),
    ] {
        assert!(
            matches!(&error, SiteResetTransactionError::NativeCleanup(source)
                    if format!("{source:#}").contains("names site \"site-one\", not this site")),
            "{error:?}"
        );
    }
    assert!(workload.engine_runs());
    assert!(workload.directory.exists());
    assert!(fixture.site_root.join("site.yaml").exists());
    assert!(state.lock().expect("reset state").calls.is_empty());
    host.stop(&workload).await;
}

fn transaction(
    state: Arc<Mutex<ResetState>>,
    runtime_behavior: RuntimeBehavior,
    persistence_fails: bool,
) -> SiteResetTransaction {
    SiteResetTransaction::new(
        Box::new(TestRuntimeCleanup {
            state: Arc::clone(&state),
            behavior: Mutex::new(runtime_behavior),
        }),
        Box::new(TestPersistenceCleanup {
            state: Arc::clone(&state),
            fails: persistence_fails,
        }),
        InitialSiteInputWriter::new(SiteInputBundleValidator::for_test()),
    )
    .with_publisher(Box::new(TestPublisher {
        state,
        inner: InitialSiteInputWriter::new(SiteInputBundleValidator::for_test()),
    }))
}

fn existing_site_fixture(site_id: &str) -> SiteLockFixture {
    let fixture = SiteLockFixture::new();
    fs::write(
        fixture.site_root.join("site.yaml"),
        format!("siteId: {site_id}\nmodelDeployments: []\n"),
    )
    .expect("existing site YAML");
    fs::write(fixture.site_root.join("node.yaml"), "retained node").expect("existing node input");
    fs::write(fixture.site_root.join("state.pb"), "retained state")
        .expect("existing current state");
    fixture
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ResetState {
    calls: Vec<String>,
    runtime_resources: Vec<String>,
}

#[derive(Clone, Copy)]
enum RuntimeBehavior {
    Success,
    FailWithoutMutation,
    RemoveOneThenFailOnce,
}

struct TestRuntimeCleanup {
    state: Arc<Mutex<ResetState>>,
    behavior: Mutex<RuntimeBehavior>,
}

impl SiteRuntimeCleanup for TestRuntimeCleanup {
    fn inventory<'a>(
        &'a self,
        site_id: &'a str,
        _scope: &'a dotcore_runtime::SiteRuntimeScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<dotcore_runtime::SiteRuntimeInventory>>
                + Send
                + 'a,
        >,
    > {
        let mut state = self.state.lock().expect("reset state");
        state.calls.push(format!("inventory:{site_id}"));
        let resources = state
            .runtime_resources
            .iter()
            .map(|name| dotcore_runtime::SiteRuntimeResource {
                runtime: "Test Runtime",
                description: name.clone(),
            })
            .collect();
        Box::pin(std::future::ready(Ok(
            dotcore_runtime::SiteRuntimeInventory {
                resources,
                skipped: Vec::new(),
            },
        )))
    }

    fn cleanup<'a>(
        &'a self,
        site_id: &'a str,
        _scope: &'a dotcore_runtime::SiteRuntimeScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<dotcore_runtime::SiteRuntimeCleanupReport>,
                > + Send
                + 'a,
        >,
    > {
        let mut state = self.state.lock().expect("reset state");
        let mut behavior = self.behavior.lock().expect("Runtime behavior");
        state.calls.push(format!("runtime:{site_id}"));
        let result = match *behavior {
            RuntimeBehavior::Success => {
                state.runtime_resources.clear();
                Ok(())
            }
            RuntimeBehavior::FailWithoutMutation => {
                Err(anyhow::anyhow!("injected runtime cleanup failure"))
            }
            RuntimeBehavior::RemoveOneThenFailOnce => {
                if state.runtime_resources.is_empty() {
                    Ok(())
                } else {
                    state.runtime_resources.remove(0);
                    if state.runtime_resources.is_empty() {
                        Ok(())
                    } else {
                        *behavior = RuntimeBehavior::Success;
                        Err(anyhow::anyhow!("injected partial runtime cleanup"))
                    }
                }
            }
        };
        Box::pin(std::future::ready(result.map(|()| Default::default())))
    }
}

struct TestPersistenceCleanup {
    state: Arc<Mutex<ResetState>>,
    fails: bool,
}

impl SitePersistenceCleanup for TestPersistenceCleanup {
    fn preflight<'a>(
        &'a self,
        _site_root: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn cleanup<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        self.state
            .lock()
            .expect("reset state")
            .calls
            .push("persistence".to_owned());
        let result = if self.fails {
            Err(anyhow::anyhow!("injected persistence cleanup failure"))
        } else {
            match fs::remove_dir_all(site_root) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(anyhow::Error::new(error))
                }
                _ => Ok(()),
            }
        };
        Box::pin(std::future::ready(result))
    }
}

struct TestPublisher {
    state: Arc<Mutex<ResetState>>,
    inner: InitialSiteInputWriter,
}

impl SiteResetPublisher for TestPublisher {
    fn publish(
        &self,
        site_root: &Path,
        inputs: &crate::initial::inputs::InitialSiteInputs,
        lock: &SiteResetLock,
    ) -> Result<(), InitialSiteWriteError> {
        self.state
            .lock()
            .expect("reset state")
            .calls
            .push("publish".to_owned());
        self.inner.create_under_reset_lock(site_root, inputs, lock)
    }
}

struct UncertainPublisher {
    state: Arc<Mutex<ResetState>>,
}

impl SiteResetPublisher for UncertainPublisher {
    fn publish(
        &self,
        site_root: &Path,
        _inputs: &crate::initial::inputs::InitialSiteInputs,
        _lock: &SiteResetLock,
    ) -> Result<(), InitialSiteWriteError> {
        self.state
            .lock()
            .expect("reset state")
            .calls
            .push("publish".to_owned());
        fs::create_dir(site_root).expect("uncertain visible site root");
        fs::write(site_root.join("publication-uncertain"), "visible")
            .expect("uncertain visible publication");
        Err(InitialSiteWriteError::OutcomeUncertain(anyhow::anyhow!(
            "injected publication confirmation failure"
        )))
    }
}

fn wait_for_file(path: &Path, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        if let Some(status) = child.try_wait().expect("helper process status") {
            panic!("shared-lock helper exited before readiness: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("shared-lock helper did not become ready");
}

#[derive(Debug, PartialEq, Eq)]
struct SiteSnapshot {
    entries: Vec<String>,
    marker: String,
    lock_metadata: String,
}

struct SiteLockFixture {
    root: PathBuf,
    site_root: PathBuf,
}

impl SiteLockFixture {
    fn new() -> Self {
        let fixture = Self::new_without_site();
        fs::create_dir(&fixture.site_root).expect("Site fixture");
        fixture
    }

    fn new_without_site() -> Self {
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-site-reset-lock-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let site_root = root.join("site");
        fs::create_dir_all(&root).expect("fixture root");
        Self { root, site_root }
    }

    fn lock_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.lock", self.site_root.display()))
    }

    fn site_tree(&self) -> Vec<(String, Option<Vec<u8>>)> {
        fn collect(root: &Path, current: &Path, entries: &mut Vec<(String, Option<Vec<u8>>)>) {
            if !current.exists() {
                return;
            }
            let mut children = fs::read_dir(current)
                .expect("tree directory")
                .map(|entry| entry.expect("tree entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for path in children {
                let relative = path
                    .strip_prefix(root)
                    .expect("relative tree path")
                    .to_string_lossy()
                    .into_owned();
                if path.is_dir() {
                    entries.push((format!("{relative}/"), None));
                    collect(root, &path, entries);
                } else {
                    entries.push((relative, Some(fs::read(&path).expect("tree file"))));
                }
            }
        }

        let mut entries = Vec::new();
        collect(&self.site_root, &self.site_root, &mut entries);
        entries
    }

    fn snapshot(&self) -> SiteSnapshot {
        let mut entries = fs::read_dir(&self.root)
            .expect("fixture root")
            .map(|entry| {
                entry
                    .expect("fixture entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        entries.sort();
        SiteSnapshot {
            entries,
            marker: fs::read_to_string(self.site_root.join("marker")).expect("Site marker"),
            lock_metadata: fs::read_to_string(self.lock_path()).expect("lock metadata"),
        }
    }
}

impl Drop for SiteLockFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_file(self.lock_path());
    }
}
