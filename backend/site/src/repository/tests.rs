use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::anyhow;
use dotcore_domain::SiteNodeId;
use dotcore_domain::test_support::tracing::{Record, capture_records};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ResolvedApplicationDeploymentSnapshot,
    },
    catalog::v1::{ModelParallelShape, OciPlatform},
    deployment::v1::ExactPlacement,
    inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentSnapshot,
        model_deployment_snapshot,
    },
    site::v1::{
        ApplicationRuntimeBinding, ExecutionTarget, ModelRuntimeBinding,
        SimulatedApplicationRuntimeAdapter, SimulatedRuntimeAdapter, SimulatedRuntimeScenario,
        Site, SiteManagerRole, SiteNode, application_runtime_binding, execution_target,
        model_runtime_binding,
    },
};
use dotcore_proto::yaml;

use super::{SiteFileWriter, SiteRepository, SiteRepositoryError, SiteWriteFault};
use crate::{
    persistence::SiteStorageError,
    view::contract::{MAX_APPLICATION_ASSIGNMENTS, MAX_MODEL_ASSIGNMENTS, MAX_NODE_VIEW_BYTES},
};

#[tokio::test]
async fn persistence_failure_retains_notifies_authoritative_memory_and_allows_follow_up_mutations()
{
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let writes = Arc::new(Mutex::new(0));
    let site = SiteRepository::load_with_writer(
        &path,
        Arc::new(PostRenameFailureWriter {
            writes: writes.clone(),
        }),
        crate::test_support::site_declaration_rules(),
    )
    .expect("desired site");
    let listener = site.register_change_listener();
    let candidate = site.snapshot().expect("initial desired site");

    let (first, records, _) = capture_records(|| site.replace(candidate.clone()));
    assert!(matches!(
        first,
        Err(SiteRepositoryError::Storage(
            SiteStorageError::OutcomeUncertain(_)
        ))
    ));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Event { level, fields }
            if level == "WARN"
                && fields.get("event_kind") == Some(&"\"site_store_write_uncertain\"".to_owned())
                && fields.get("storage") == Some(&"\"site.yaml\"".to_owned())
    )));
    tokio::time::timeout(std::time::Duration::from_secs(1), listener.notified())
        .await
        .expect("persistence failure still notifies reconciliation");
    let (second, _, _) = capture_records(|| site.replace(candidate.clone()));
    assert!(matches!(
        second,
        Err(SiteRepositoryError::Storage(
            SiteStorageError::OutcomeUncertain(_)
        ))
    ));
    tokio::time::timeout(std::time::Duration::from_secs(1), listener.notified())
        .await
        .expect("follow-up persistence failure still notifies reconciliation");
    assert_eq!(*writes.lock().expect("write count"), 2);
    assert_eq!(site.snapshot().expect("authoritative memory"), candidate);
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test]
async fn accepted_site_replacement_notifies_all_controller_listeners() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let model_listener = site.register_change_listener();
    let application_listener = site.register_change_listener();

    site.replace(site.snapshot().expect("Site snapshot"))
        .expect("accepted replacement");

    tokio::time::timeout(std::time::Duration::from_secs(1), model_listener.notified())
        .await
        .expect("Model controller notification");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        application_listener.notified(),
    )
    .await
    .expect("Apps controller notification");
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test]
async fn mutation_writes_before_notifying_on_success_and_failure() {
    assert_mutation_writes_before_notifying(false).await;
    assert_mutation_writes_before_notifying(true).await;
}

async fn assert_mutation_writes_before_notifying(fails: bool) {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let write_started = Arc::new(std::sync::Barrier::new(2));
    let release_write = Arc::new(std::sync::Barrier::new(2));
    let site = SiteRepository::load_with_writer(
        &path,
        Arc::new(BlockingFailureWriter {
            write_started: Arc::clone(&write_started),
            release_write: Arc::clone(&release_write),
            fails,
        }),
        crate::test_support::site_declaration_rules(),
    )
    .expect("desired site");
    let listener = site.register_change_listener();
    let candidate = site.snapshot().expect("Site snapshot");
    let mutation = tokio::task::spawn_blocking(move || site.replace(candidate));

    write_started.wait();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), listener.notified())
            .await
            .is_err(),
        "listeners must not run before persistence finishes"
    );
    release_write.wait();
    let result = mutation.await.expect("mutation task");
    if fails {
        assert!(matches!(
            result,
            Err(SiteRepositoryError::Storage(SiteStorageError::Storage(_)))
        ));
    } else {
        result.expect("successful mutation");
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), listener.notified())
        .await
        .expect("post-write notification");
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn model_identity_reservations_preserve_uncertain_and_unused_gaps_across_restart() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");

    let failed = SiteRepository::load_with_writer(
        &path,
        Arc::new(PreRenameFailureWriter),
        crate::test_support::site_declaration_rules(),
    )
    .expect("failure-injected site");
    assert!(matches!(
        failed.reserve_model_deployment_id(),
        Err(SiteRepositoryError::Storage(SiteStorageError::Storage(_)))
    ));
    assert_eq!(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("restart after known failure")
            .snapshot()
            .expect("known-failure site")
            .model_deployment_identity_allocator
            .expect("allocator")
            .next_sequence,
        1
    );

    let uncertain = SiteRepository::load_with_writer(
        &path,
        Arc::new(PostRenameFailureWriter {
            writes: Arc::new(Mutex::new(0)),
        }),
        crate::test_support::site_declaration_rules(),
    )
    .expect("uncertain site");
    assert!(matches!(
        uncertain.reserve_model_deployment_id(),
        Err(SiteRepositoryError::Storage(
            SiteStorageError::OutcomeUncertain(_)
        ))
    ));
    let restarted = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("restart after uncertain reservation");
    let second = restarted
        .reserve_model_deployment_id()
        .expect("reservation after restart");
    assert_eq!(second, "model-0123456789abcdef-2");

    drop(restarted);
    let restarted = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("restart after unused reservation");
    let third = restarted
        .reserve_model_deployment_id()
        .expect("reservation after unused gap");
    assert_eq!(third, "model-0123456789abcdef-3");
    assert_ne!(second, third);
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn accepted_model_snapshot_round_trips_exactly_through_site_yaml_restart() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let deployment = model_deployment("deployment-one", "model-one");

    site.accept_model_deployment_exposure(deployment.clone(), None)
        .expect("accepted model snapshot");

    let persisted = fs::read_to_string(&path).expect("persisted site.yaml");
    assert!(persisted.contains("engineKind: ENGINE_KIND_LLAMA_CPP"));
    assert!(persisted.contains("semanticsVersion: 2"));
    let restarted = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("restarted desired site")
        .snapshot()
        .expect("restarted snapshot");
    assert_eq!(restarted.model_deployments, [deployment]);
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn route_mutation_round_trips_exact_declarations_without_mutating_model_records() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let original = site_with_deployments();
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let route = shared_route();

    site.mutate_inference_routes(|transaction| {
        assert_eq!(transaction.site_id, "site-one");
        assert_eq!(transaction.deployments, original.model_deployments);
        transaction.routes.push(route.clone());
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("accepted route-manager mutation");

    assert_eq!(
        site.snapshot().expect("accepted site").model_deployments,
        original.model_deployments
    );
    assert_eq!(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("reopened desired site")
            .snapshot()
            .expect("reopened snapshot")
            .inference_routes,
        [route]
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn route_deletion_winner_rejects_late_application_binding_persistence() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = site_with_deployment();
    original.inference_routes = vec![sole_member_route()];
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    site.mutate_inference_routes(|transaction| {
        transaction
            .routes
            .retain(|route| route.inference_route_id != "route-one");
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("route deletion wins");
    let before_binding_persistence = site.snapshot().expect("Site after route deletion");
    let binding_persistence = site.mutate_application_deployments(|deployments| {
        deployments.push(route_bound_application("application-one"));
        Ok::<(), std::convert::Infallible>(())
    });
    let after_binding_persistence = site.snapshot().expect("Site after rejected binding");

    assert!(matches!(
        binding_persistence,
        Err(super::ScopedSiteMutationError::Repository(
            SiteRepositoryError::InvalidSite(message)
        )) if message.to_string().contains("references missing inference route")
    ));
    assert_eq!(after_binding_persistence, before_binding_persistence);
    assert!(after_binding_persistence.application_deployments.is_empty());
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn route_persistence_failure_retains_the_accepted_in_memory_declaration() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let original = site_with_deployment();
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load_with_writer(
        &path,
        Arc::new(PostRenameFailureWriter {
            writes: Arc::new(Mutex::new(0)),
        }),
        crate::test_support::site_declaration_rules(),
    )
    .expect("desired site");
    let route = sole_member_route();

    assert!(matches!(
        site.mutate_inference_routes(|transaction| {
            transaction.routes.push(route.clone());
            Ok::<(), std::convert::Infallible>(())
        }),
        Err(super::ScopedSiteMutationError::Repository(
            SiteRepositoryError::Storage(SiteStorageError::OutcomeUncertain(_))
        ))
    ));
    assert_eq!(
        site.snapshot()
            .expect("authoritative site")
            .inference_routes,
        [route]
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn route_conflict_rolls_back_both_owner_sections() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let original = site_with_deployment();
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    site.mutate_inference_routes(|transaction| {
        transaction.routes.push(sole_member_route());
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("existing sole-member route");
    let before = site.snapshot().expect("Site before conflicting load");
    let deployment = model_deployment("deployment-two", "model-two");
    let mut conflicting_route = sole_member_route();
    conflicting_route.inference_route_id = "route-two".to_owned();
    conflicting_route.fallback_groups[0].model_deployment_ids =
        vec![deployment.model_deployment_id.clone()];

    let conflict = site.accept_model_deployment_exposure(deployment, Some(conflicting_route));
    let after = site.snapshot().expect("Site after conflicting load");
    fs::remove_dir_all(directory).expect("fixture cleanup");

    assert!(matches!(conflict, Err(SiteRepositoryError::InvalidSite(_))));
    assert_eq!(after, before);
}

#[tokio::test]
async fn deployment_unload_reports_sorted_application_route_blockers_without_mutation_or_notification()
 {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let deployment = model_deployment("deployment-one", "model-one");
    site.accept_model_deployment_exposure(deployment, Some(sole_member_route()))
        .expect("focused exposure acceptance");
    site.mutate_application_deployments(|deployments| {
        let mut teardown = route_bound_application("application-three");
        teardown.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
        let present = route_bound_application("application-one");
        let mut paused = route_bound_application("application-two");
        paused.desired_state = ApplicationDeploymentDesiredState::Paused as i32;
        deployments.extend([teardown, present, paused]);
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Application bindings");
    let listener = site.register_change_listener();
    let before = site.snapshot().expect("Site before unload");
    let persisted_before = fs::read(&path).expect("persisted site before unload");

    let result = site.accept_model_deployment_unload("deployment-one", 2);
    let after = site.snapshot().expect("Site after rejected unload");
    let persisted_after = fs::read(&path).expect("persisted site after rejected unload");

    let blockers = match result {
        Err(super::ModelUnloadAcceptanceError::ApplicationRouteReferences(blockers)) => blockers,
        other => panic!("expected application route references, got {other:?}"),
    };
    assert_eq!(
        blockers,
        [
            super::ApplicationRouteReference {
                application_deployment_id: "application-one".to_owned(),
                application_deployment_name: "Application application-one".to_owned(),
                inference_route_id: "route-one".to_owned(),
                inference_route_name: "route-one".to_owned(),
            },
            super::ApplicationRouteReference {
                application_deployment_id: "application-three".to_owned(),
                application_deployment_name: "Application application-three".to_owned(),
                inference_route_id: "route-one".to_owned(),
                inference_route_name: "route-one".to_owned(),
            },
            super::ApplicationRouteReference {
                application_deployment_id: "application-two".to_owned(),
                application_deployment_name: "Application application-two".to_owned(),
                inference_route_id: "route-one".to_owned(),
                inference_route_name: "route-one".to_owned(),
            },
        ]
    );
    assert_eq!(after, before);
    assert_eq!(persisted_after, persisted_before);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), listener.notified())
            .await
            .is_err()
    );
    site.mutate_application_deployments(|deployments| {
        deployments.clear();
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Application deletion");

    let accepted = site
        .accept_model_deployment_unload("deployment-one", 2)
        .expect("model teardown after application deletion");
    let accepted_site = site.snapshot().expect("Site after accepted unload");

    assert_eq!(
        accepted.desired_state,
        ModelDeploymentDesiredState::Teardown as i32
    );
    assert!(
        accepted_site
            .inference_routes
            .iter()
            .all(|route| route.inference_route_id != "route-one")
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn deployment_unload_removes_only_its_sole_member_routes() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = site_with_deployments();
    let mut unrelated = sole_member_route();
    unrelated.inference_route_id = "route-two".to_owned();
    unrelated.name = "route-two".to_owned();
    unrelated.fallback_groups[0].model_deployment_ids = vec!["deployment-two".to_owned()];
    original.inference_routes = vec![sole_member_route(), unrelated.clone()];
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");

    let accepted = site
        .accept_model_deployment_unload("deployment-one", 2)
        .expect("model teardown with its sole-member route");
    let after = site.snapshot().expect("Site after accepted unload");
    fs::remove_dir_all(directory).expect("fixture cleanup");

    assert_eq!(accepted.model_deployment_id, "deployment-one");
    assert_eq!(
        accepted.desired_state,
        ModelDeploymentDesiredState::Teardown as i32
    );
    assert_eq!(after.inference_routes, [unrelated]);
    assert_eq!(after.model_deployments[0], accepted);
    assert_eq!(
        after.model_deployments[1..],
        original.model_deployments[1..]
    );
}

#[tokio::test]
async fn deployment_unload_reports_shared_routes_before_application_blockers_without_mutation() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = site_with_deployments();
    let mut shared = shared_route();
    shared.inference_route_id = "route-shared".to_owned();
    shared.name = "shared-route".to_owned();
    original.inference_routes = vec![sole_member_route(), shared];
    original.application_deployments = vec![route_bound_application("application-one")];
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let listener = site.register_change_listener();
    let before = site.snapshot().expect("Site before unload");
    let persisted_before = fs::read(&path).expect("persisted site before unload");

    let result = site.accept_model_deployment_unload("deployment-one", 2);
    let after = site.snapshot().expect("Site after rejected unload");
    let persisted_after = fs::read(&path).expect("persisted site after rejected unload");
    fs::remove_dir_all(directory).expect("fixture cleanup");

    assert!(
        matches!(
            &result,
            Err(super::ModelUnloadAcceptanceError::SharedRouteReferences(names))
                if names == &["shared-route"]
        ),
        "expected only the shared route blocker, got {result:?}"
    );
    assert_eq!(after, before);
    assert_eq!(persisted_after, persisted_before);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), listener.notified())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn observers_never_receive_a_partial_exposure_pair() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let listener = site.register_change_listener();
    let observer_site = site.clone();
    let observer = tokio::spawn(async move {
        listener.notified().await;
        observer_site.snapshot().expect("observed site")
    });
    let deployment = model_deployment("deployment-one", "model-one");

    site.accept_model_deployment_exposure(deployment, Some(sole_member_route()))
        .expect("focused exposure acceptance");
    let observed = observer.await.expect("observer task");
    let accepted = site.snapshot().expect("accepted exposure pair");
    fs::remove_dir_all(directory).expect("fixture cleanup");

    assert_eq!(observed, accepted);
}

#[test]
fn composite_persistence_failure_retains_the_complete_pair_in_authoritative_memory() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&valid_site()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load_with_writer(
        &path,
        Arc::new(PostRenameFailureWriter {
            writes: Arc::new(Mutex::new(0)),
        }),
        crate::test_support::site_declaration_rules(),
    )
    .expect("desired site");
    let deployment = model_deployment("deployment-one", "model-one");

    let result =
        site.accept_model_deployment_exposure(deployment.clone(), Some(sole_member_route()));
    let accepted = site.snapshot().expect("authoritative site");
    fs::remove_dir_all(directory).expect("fixture cleanup");

    assert!(matches!(
        result,
        Err(SiteRepositoryError::Storage(
            SiteStorageError::OutcomeUncertain(_)
        ))
    ));
    assert_eq!(accepted.model_deployments, [deployment]);
    assert_eq!(accepted.inference_routes, [sole_member_route()]);
}

#[test]
fn model_and_route_mutations_cannot_write_each_others_owner_sections() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let original = site_with_deployment();
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    site.mutate_inference_routes(|transaction| {
        transaction.routes.push(sole_member_route());
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Route-manager mutation");
    let route_owned = site
        .snapshot()
        .expect("Route-manager snapshot")
        .inference_routes;

    site.mutate_model_deployments(|transaction| {
        transaction
            .deployments
            .push(model_deployment("deployment-two", "model-two"));
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Model-owner mutation");
    let after_model = site.snapshot().expect("Model-owner snapshot");
    assert_eq!(after_model.inference_routes, route_owned);

    site.mutate_inference_routes(|transaction| {
        transaction.routes.clear();
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Route-manager deletion");
    let after_route = site.snapshot().expect("Route-manager snapshot");
    fs::remove_dir_all(directory).expect("fixture cleanup");

    assert_eq!(after_route.model_deployments, after_model.model_deployments);
    assert_eq!(after_route.inference_routes, []);
}

#[tokio::test]
async fn metadata_mutations_are_owner_scoped_and_do_not_notify_reconciliation() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = site_with_deployment();
    let mut application = route_bound_application("application-one");
    application.application_deployment_name = "Application One".to_owned();
    application.inference_route_id = None;
    original.application_deployments = vec![application];
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let mut expected_after_model = original.clone();
    expected_after_model.model_deployments[0].model_deployment_name = "Renamed Model".to_owned();
    let mut expected_after_application = expected_after_model.clone();
    expected_after_application.application_deployments[0].application_deployment_name =
        "Renamed Application".to_owned();
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let model_listener = site.register_change_listener();
    let application_listener = site.register_change_listener();

    site.mutate_model_deployment_metadata(|transaction| {
        transaction.deployments[0].model_deployment_name = "Renamed Model".to_owned();
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Model metadata mutation");
    let after_model = site.snapshot().expect("Site after model metadata mutation");

    assert_eq!(
        after_model.model_deployments[0].model_deployment_name,
        "Renamed Model"
    );
    assert_eq!(after_model, expected_after_model);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            model_listener.notified()
        )
        .await
        .is_err(),
        "Model metadata must not notify the model reconciliation listener"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            application_listener.notified(),
        )
        .await
        .is_err(),
        "Model metadata must not notify the apps reconciliation listener"
    );

    site.mutate_application_deployment_metadata(|deployments| {
        deployments[0].application_deployment_name = "Renamed Application".to_owned();
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("Application metadata mutation");
    let after_application = site
        .snapshot()
        .expect("Site after application metadata mutation");

    assert_eq!(
        after_application.application_deployments[0].application_deployment_name,
        "Renamed Application"
    );
    assert_eq!(after_application, expected_after_application);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            model_listener.notified()
        )
        .await
        .is_err(),
        "Application metadata must not notify the model reconciliation listener"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            application_listener.notified(),
        )
        .await
        .is_err(),
        "Application metadata must not notify the apps reconciliation listener"
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn failed_and_uncertain_writes_including_metadata_leave_the_site_dirty() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let original = site_with_deployment();
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let (site, faults) =
        SiteRepository::load_with_write_faults_for_test(&path).expect("desired site");
    let rename = |name: &str| {
        site.mutate_model_deployment_metadata(|transaction| {
            transaction.deployments[0].model_deployment_name = name.to_owned();
            Ok::<(), std::convert::Infallible>(())
        })
    };

    faults.push(SiteWriteFault::Storage);
    assert!(rename("Failed Metadata").is_err());
    assert!(site.is_dirty());
    assert_eq!(
        fs::read_to_string(&path).expect("site.yaml"),
        yaml::to_string(&original).expect("YAML")
    );
    assert_eq!(
        site.snapshot().expect("accepted memory").model_deployments[0].model_deployment_name,
        "Failed Metadata"
    );

    rename("Durable Metadata").expect("confirmed metadata write");
    assert!(!site.is_dirty());

    faults.push(SiteWriteFault::Uncertain);
    assert!(matches!(
        rename("Uncertain Metadata"),
        Err(super::ScopedSiteMutationError::Repository(
            SiteRepositoryError::Storage(SiteStorageError::OutcomeUncertain(_))
        ))
    ));
    assert!(site.is_dirty());

    faults.push(SiteWriteFault::Storage);
    assert!(
        site.mutate_model_deployments(|transaction| {
            transaction.deployments[0].lifecycle_generation = 2;
            Ok::<(), std::convert::Infallible>(())
        })
        .is_err()
    );
    assert!(site.is_dirty());
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn node_view_assignment_growth_past_the_wire_limit_is_rejected_before_acceptance() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = node_site();
    original.model_deployments = (0..MAX_MODEL_ASSIGNMENTS)
        .map(|index| node_deployment(&format!("deployment-{index}"), 0))
        .collect();
    fs::write(&path, yaml::to_string(&original).expect("YAML")).expect("Site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let notifications = counted_notifications(&site);
    let before = fs::read(&path).expect("site.yaml");

    let rejected =
        site.accept_model_deployment_exposure(node_deployment("deployment-extra", 0), None);

    assert!(matches!(
        rejected,
        Err(SiteRepositoryError::NodeViewLimitExceeded { node_id }) if node_id == "worker-one"
    ));
    assert_eq!(site.snapshot().expect("accepted memory"), original);
    assert_eq!(fs::read(&path).expect("site.yaml"), before);
    assert!(!site.is_dirty());
    assert_eq!(notifications.load(Ordering::SeqCst), 0);

    let mut manager_hosted = model_deployment("deployment-manager", "model-one");
    manager_hosted.model_deployment_name = "Manager hosted".to_owned();
    site.accept_model_deployment_exposure(manager_hosted, None)
        .expect("another node's view stays within its limits");
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn the_manager_node_view_is_bound_by_the_same_assignment_limit() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = valid_site();
    original.model_deployments = (0..MAX_MODEL_ASSIGNMENTS)
        .map(|index| model_deployment(&format!("deployment-{index}"), "model-one"))
        .collect();
    fs::write(&path, yaml::to_string(&original).expect("YAML")).expect("Site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");

    let rejected = site
        .accept_model_deployment_exposure(model_deployment("deployment-extra", "model-one"), None);

    assert!(matches!(
        rejected,
        Err(SiteRepositoryError::NodeViewLimitExceeded { node_id }) if node_id == "node-one"
    ));
    assert_eq!(site.snapshot().expect("accepted memory"), original);
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn application_assignment_growth_past_the_wire_limit_is_rejected_before_acceptance() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = node_site();
    let mut application_target = original.execution_targets[1].clone();
    application_target.execution_target_id = "worker-application-target".to_owned();
    application_target.site_node_id = "worker-one".to_owned();
    original.execution_targets.push(application_target);
    original.application_deployments = (0..MAX_APPLICATION_ASSIGNMENTS)
        .map(|index| node_application(&format!("application-{index}")))
        .collect();
    fs::write(&path, yaml::to_string(&original).expect("YAML")).expect("Site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");
    let notifications = counted_notifications(&site);
    let before = fs::read(&path).expect("site.yaml");

    let rejected = site.mutate_application_deployments(|deployments| {
        deployments.push(node_application("application-extra"));
        Ok::<(), std::convert::Infallible>(())
    });

    assert!(matches!(
        rejected,
        Err(super::ScopedSiteMutationError::Repository(
            SiteRepositoryError::NodeViewLimitExceeded { node_id }
        )) if node_id == "worker-one"
    ));
    assert_eq!(site.snapshot().expect("accepted memory"), original);
    assert_eq!(fs::read(&path).expect("site.yaml"), before);
    assert_eq!(notifications.load(Ordering::SeqCst), 0);
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn node_view_byte_growth_is_rejected_while_an_oversized_view_may_still_shrink() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let oversized = MAX_NODE_VIEW_BYTES / 4 + 1024;
    let mut original = node_site();
    original.model_deployments = (0..5)
        .map(|index| node_deployment(&format!("deployment-{index}"), oversized))
        .collect();
    fs::write(&path, yaml::to_string(&original).expect("YAML")).expect("Site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site loaded over the limit");
    let before = fs::read(&path).expect("site.yaml");

    let renamed = site.mutate_model_deployment_metadata(|transaction| {
        transaction.deployments[0].model_deployment_name = "Longer model name".to_owned();
        Ok::<_, std::convert::Infallible>(())
    });
    let created =
        site.accept_model_deployment_exposure(node_deployment("deployment-extra", oversized), None);

    assert!(matches!(
        renamed,
        Err(super::ScopedSiteMutationError::Repository(
            SiteRepositoryError::NodeViewLimitExceeded { .. }
        ))
    ));
    assert!(matches!(
        created,
        Err(SiteRepositoryError::NodeViewLimitExceeded { .. })
    ));
    assert_eq!(site.snapshot().expect("accepted memory"), original);
    assert_eq!(fs::read(&path).expect("site.yaml"), before);

    site.mutate_model_deployments(|transaction| {
        transaction.deployments.pop();
        Ok::<_, std::convert::Infallible>(())
    })
    .expect("a shrinking mutation of an oversized view is accepted");
    assert_eq!(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("restarted site")
            .snapshot()
            .expect("restarted site snapshot")
            .model_deployments,
        original.model_deployments[..4]
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn teardown_of_an_oversized_view_is_accepted_when_its_generation_widens() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let oversized = MAX_NODE_VIEW_BYTES / 4 + 1024;
    let mut original = node_site();
    original.model_deployments = (0..5)
        .map(|index| {
            let mut deployment = node_deployment(&format!("deployment-{index}"), oversized);
            deployment.lifecycle_generation = 127;
            deployment
        })
        .collect();
    fs::write(&path, yaml::to_string(&original).expect("YAML")).expect("Site fixture");
    let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site loaded over the limit");

    let teardown = site
        .accept_model_deployment_unload("deployment-0", 128)
        .expect("a teardown is never rejected by the node-view growth check");

    assert_eq!(teardown.lifecycle_generation, 128);
    assert_eq!(
        teardown.desired_state,
        ModelDeploymentDesiredState::Teardown as i32
    );
    assert_eq!(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("restarted site")
            .snapshot()
            .expect("restarted site snapshot")
            .model_deployments[0],
        teardown
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn unrelated_owner_write_persists_a_failed_model_mutation_and_clears_dirty() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut original = site_with_deployment();
    let mut application = route_bound_application("application-one");
    application.inference_route_id = None;
    original.application_deployments = vec![application];
    fs::write(
        &path,
        yaml::to_string(&original).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let (site, faults) =
        SiteRepository::load_with_write_faults_for_test(&path).expect("desired site");

    faults.push(SiteWriteFault::Storage);
    assert!(
        site.mutate_model_deployments(|transaction| {
            transaction.deployments[0].model_deployment_name = "Accepted Model".to_owned();
            Ok::<(), std::convert::Infallible>(())
        })
        .is_err()
    );
    assert!(site.is_dirty());
    site.mutate_application_deployment_metadata(|deployments| {
        deployments[0].application_deployment_name = "Renamed Application".to_owned();
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("unrelated owner write");
    let restarted = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("restarted site")
        .snapshot()
        .expect("restarted site snapshot");

    assert!(!site.is_dirty());
    assert_eq!(restarted, site.snapshot().expect("accepted memory"));
    assert_eq!(
        restarted.model_deployments[0].model_deployment_name,
        "Accepted Model"
    );
    assert_eq!(
        restarted.application_deployments[0].application_deployment_name,
        "Renamed Application"
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn desired_site_rewrite_preserves_site_node_declarations() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let site = valid_site();
    fs::write(&path, yaml::to_string(&site).expect("desired site YAML"))
        .expect("desired site fixture");
    let desired = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site");

    desired
        .replace(desired.snapshot().expect("desired site snapshot"))
        .expect("rewrite desired site");

    let rewritten: Site = yaml::from_file(&path).expect("rewritten desired site");
    assert_eq!(rewritten.nodes, site.nodes);
    assert!(rewritten.nodes[0].site_manager.is_some());
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn desired_site_errors_name_the_yaml_and_storage_categories() {
    let yaml = SiteRepositoryError::Yaml(dotcore_proto::yaml::RestrictedYamlError::DocumentCount {
        count: 2,
        location: None,
    });
    let storage = SiteRepositoryError::Storage(SiteStorageError::Storage(anyhow!(
        "site.yaml replacement failed"
    )));

    assert_eq!(
        yaml.to_string(),
        "Desired site YAML failed: Unsupported YAML document count: found 2 YAML documents"
    );
    assert_eq!(
        storage.to_string(),
        "Desired site storage failed: site storage failed: site.yaml replacement failed"
    );
}

#[test]
fn owner_rule_rejection_fails_the_load_naming_the_cause_and_reset() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        yaml::to_string(&site_with_deployment()).expect("desired site YAML"),
    )
    .expect("desired site fixture");
    let rejecting = crate::validation::SiteDeclarationRules {
        validate_local_model_declaration: |_| {
            Err(dotcore_catalog::CatalogError::Invalid(anyhow::anyhow!(
                "artifact selection retains no preparation policy"
            )))
        },
    };

    let Err(error) = SiteRepository::load(&path, rejecting) else {
        panic!("Site with a rejected model snapshot loaded");
    };

    assert_eq!(
        error.to_string(),
        "Invalid desired site: site.yaml active deployment \"deployment-one\" has an invalid immutable model snapshot; remove the deployment and create it again, or reset the site"
    );
    assert_eq!(
        anyhow::Error::new(error).root_cause().to_string(),
        "artifact selection retains no preparation policy"
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[test]
fn application_owned_policy_value_name_fails_the_load_naming_the_cause_and_reset() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let mut site = valid_site();
    site.application_deployments = vec![ApplicationDeploymentDeclaration {
            lifecycle_generation: 1,
            application_deployment_id: "application-one".to_owned(),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                service_selections: vec![
                    dotcore_proto::dotcore::application::v1::ResolvedApplicationServiceSelection {
                        service_name: "web".to_owned(),
                        preparation_policy:
                            dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy::LocalDevelopment
                                as i32,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        }];
    let encoded = yaml::to_string(&site).expect("desired site YAML");
    assert!(encoded.contains("preparationPolicy: ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT"));
    fs::write(
        &path,
        encoded.replace(
            "preparationPolicy: ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT",
            "preparationPolicy: APPLICATION_ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT",
        ),
    )
    .expect("desired site fixture");

    let Err(error) = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
    else {
        panic!("Site with an application-owned policy value name loaded");
    };

    assert!(matches!(error, SiteRepositoryError::SchemaMismatch(_)));
    assert_eq!(
        error.to_string(),
        "site.yaml does not match the site schema of this build (unknown variant `APPLICATION_ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT`, expected one of `ARTIFACT_PREPARATION_POLICY_UNSPECIFIED`, `ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE`, `ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT`, `ARTIFACT_PREPARATION_POLICY_DEVELOPMENT_SOURCE`); correct the named value or reset the site"
    );
    fs::remove_dir_all(directory).expect("fixture cleanup");
}

struct PostRenameFailureWriter {
    writes: Arc<Mutex<usize>>,
}

struct PreRenameFailureWriter;

struct BlockingFailureWriter {
    write_started: Arc<std::sync::Barrier>,
    release_write: Arc<std::sync::Barrier>,
    fails: bool,
}

impl SiteFileWriter for BlockingFailureWriter {
    fn replace(&self, _: &Path, _: &str) -> Result<(), SiteStorageError> {
        self.write_started.wait();
        self.release_write.wait();
        if self.fails {
            Err(SiteStorageError::Storage(anyhow!("injected write failure")))
        } else {
            Ok(())
        }
    }
}

impl SiteFileWriter for PreRenameFailureWriter {
    fn replace(&self, _: &Path, _: &str) -> Result<(), SiteStorageError> {
        Err(SiteStorageError::Storage(anyhow!(
            "injected pre-rename failure"
        )))
    }
}

impl SiteFileWriter for PostRenameFailureWriter {
    fn replace(&self, path: &Path, contents: &str) -> Result<(), SiteStorageError> {
        fs::write(path, contents).expect("simulated atomic replacement");
        *self.writes.lock().expect("write count") += 1;
        Err(SiteStorageError::OutcomeUncertain(anyhow!(
            "injected parent directory sync failure"
        )))
    }
}

fn route_bound_application(application_deployment_id: &str) -> ApplicationDeploymentDeclaration {
    let placement = ExactPlacement {
        site_node_id: "node-one".to_owned(),
        execution_target_id: "application-target".to_owned(),
    };
    let platform = OciPlatform {
        operating_system: "linux".to_owned(),
        architecture: "amd64".to_owned(),
        variant: None,
    };
    ApplicationDeploymentDeclaration {
        lifecycle_generation: 1,
        application_deployment_id: application_deployment_id.to_owned(),
        application_deployment_name: format!("Application {application_deployment_id}"),
        placement: Some(placement.clone()),
        snapshot: Some(ResolvedApplicationDeploymentSnapshot {
            placement: Some(placement),
            platform: Some(platform),
            ..Default::default()
        }),
        desired_state: ApplicationDeploymentDesiredState::Present as i32,
        inference_route_id: Some("route-one".to_owned()),
        ..Default::default()
    }
}

/// A minimal application deployment placed on worker-one's application target.
fn node_application(application_deployment_id: &str) -> ApplicationDeploymentDeclaration {
    let mut application = route_bound_application(application_deployment_id);
    let placement = ExactPlacement {
        site_node_id: "worker-one".to_owned(),
        execution_target_id: "worker-application-target".to_owned(),
    };
    application.placement = Some(placement.clone());
    application.inference_route_id = None;
    if let Some(snapshot) = application.snapshot.as_mut() {
        snapshot.placement = Some(placement);
    }
    application
}

fn sole_member_route() -> InferenceRoute {
    InferenceRoute {
        inference_route_id: "route-one".to_owned(),
        name: "route-one".to_owned(),
        enabled: true,
        fallback_groups: vec![FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec!["deployment-one".to_owned()],
        }],
        generation: 1,
    }
}

fn shared_route() -> InferenceRoute {
    InferenceRoute {
        inference_route_id: "route-one".to_owned(),
        name: "route-one".to_owned(),
        enabled: false,
        fallback_groups: vec![
            FallbackGroup {
                selection: FallbackGroupSelection::RoundRobin as i32,
                model_deployment_ids: vec![
                    "deployment-one".to_owned(),
                    "deployment-two".to_owned(),
                ],
            },
            FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec!["deployment-three".to_owned()],
            },
        ],
        generation: 7,
    }
}

fn valid_site() -> Site {
    Site {
        site_id: "site-one".to_owned(),
        nodes: vec![SiteNode {
            node_id: "node-one".to_owned(),
            host: "127.0.0.1".to_owned(),
            display_name: "Node One".to_owned(),
            site_manager: Some(SiteManagerRole {}),
            manager_host: String::new(),
        }],
        model_deployment_identity_allocator: Some(
            dotcore_proto::dotcore::site::v1::ModelDeploymentIdentityAllocator {
                namespace: "0123456789abcdef".to_owned(),
                next_sequence: 1,
            },
        ),
        execution_targets: vec![
            ExecutionTarget {
                execution_target_id: "target-one".to_owned(),
                display_name: "target-one".to_owned(),
                site_node_id: SiteNodeId::for_test("node-one").to_string(),
                accelerator_device_ids: Vec::new(),
                runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                    ModelRuntimeBinding {
                        adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                            simulated_runtime(),
                        )),
                    },
                )),
                platform: Some(OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                }),
            },
            ExecutionTarget {
                execution_target_id: "application-target".to_owned(),
                display_name: "application-target".to_owned(),
                site_node_id: SiteNodeId::for_test("node-one").to_string(),
                accelerator_device_ids: Vec::new(),
                runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                    ApplicationRuntimeBinding {
                        adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                            SimulatedApplicationRuntimeAdapter {},
                        )),
                    },
                )),
                platform: Some(OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                }),
            },
        ],
        ..Default::default()
    }
}

fn site_with_deployment() -> Site {
    let mut site = valid_site();
    site.model_deployments = vec![model_deployment("deployment-one", "model-one")];
    site
}

fn site_with_deployments() -> Site {
    let mut site = valid_site();
    site.model_deployments = vec![
        model_deployment("deployment-one", "model-one"),
        model_deployment("deployment-two", "model-two"),
        model_deployment("deployment-three", "model-three"),
    ];
    site
}

fn model_deployment(model_deployment_id: &str, model_id: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: model_deployment_id.to_owned(),
        model_deployment_name: "Model".to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        }),
        requested_parallel_shape: ModelParallelShape::One as i32,
        snapshot: Some(model_snapshot(model_id)),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        upstream_desired_configuration: None,
        lifecycle_generation: 1,
        lifecycle_transition:
            dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Create as i32,
        lifecycle_recovery_required: false,
        node_resolved_launch_identities: Vec::new(),
    }
}

fn model_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    dotcore_domain::test_support::model::local_model_snapshot(model_id)
}

/// The valid site plus one node that receives its model intent over the private wire.
fn node_site() -> Site {
    let mut site = valid_site();
    site.nodes.push(SiteNode {
        node_id: "worker-one".to_owned(),
        host: "127.0.0.2".to_owned(),
        display_name: "Worker One".to_owned(),
        site_manager: None,
        manager_host: String::new(),
    });
    let mut target = site.execution_targets[0].clone();
    target.execution_target_id = "worker-target".to_owned();
    target.site_node_id = "worker-one".to_owned();
    site.execution_targets.push(target);
    site
}

/// A worker-placed deployment whose snapshot display name adds `padding` bytes to its view.
fn node_deployment(model_deployment_id: &str, padding: usize) -> ModelDeploymentDeclaration {
    let mut deployment = model_deployment(model_deployment_id, "model-one");
    deployment.placement = Some(ExactPlacement {
        site_node_id: "worker-one".to_owned(),
        execution_target_id: "worker-target".to_owned(),
    });
    if let Some(model_deployment_snapshot::Source::Local(local)) = deployment
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.source.as_mut())
    {
        local.model_display_name.push_str(&"x".repeat(padding));
    }
    deployment
}

fn counted_notifications(site: &SiteRepository) -> Arc<AtomicUsize> {
    let notifications = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notifications);
    site.register_change_observer(Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    notifications
}

fn simulated_runtime() -> SimulatedRuntimeAdapter {
    SimulatedRuntimeAdapter {
        default_scenario: Some(SimulatedRuntimeScenario::default()),
        deployment_scenarios: Vec::new(),
    }
}

fn test_directory() -> std::path::PathBuf {
    let path = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-desired-site-{}",
        dotcore_domain::test_support::unique_suffix()
    ));
    fs::create_dir(&path).expect("fixture directory");
    path
}
