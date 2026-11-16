//! Composite model deployment exposure and route deletion integrity contracts.

use std::fs;

use dotcore::composition::SelectedModelService;
use dotcore_proto::dotcore::{
    inference::v1::{
        CreateRouteRequest, FallbackGroup, FallbackGroupSelection, InferenceRoute,
        ListRoutesRequest, ReplaceRouteRequest,
    },
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        ModelDeployment, ModelDeploymentDesiredState, ModelDeploymentExposure,
        RemoveModelDeploymentRequest, model_deployment_exposure,
        model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
    },
    site::v1::Site,
};
use dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport;
use dotcore_site::initial::validation::{SiteInputBundleValidator, SiteInputValidationError};
use tonic::{Code, Request};

use crate::support;

#[tokio::test]
async fn direct_load_persists_one_deployment_and_its_direct_route() {
    let root = support::temporary_fast_fixture();
    let service = support::model_service_for(&support::configuration(&root))
        .await
        .expect("Direct model service");
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(local_source()),
            exposure: Some(direct_exposure("public-model")),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("Direct load")
    .into_inner()
    .deployment
    .expect("Direct deployment");
    let site = read_site(&root);

    assert_eq!(site.model_deployments.len(), 1);
    assert_eq!(
        site.model_deployments[0].model_deployment_id,
        deployment.model_deployment_id
    );
    assert_eq!(site.inference_routes.len(), 1);
    let route = &site.inference_routes[0];
    assert_eq!(route.name, "public-model");
    assert!(route.enabled);
    assert_eq!(
        route.fallback_groups,
        [FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![deployment.model_deployment_id],
        }]
    );
    assert_eq!(route.generation, 1);
}

#[tokio::test]
async fn direct_load_and_unload_return_with_the_route_snapshot_already_current() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let mut models = backend.model_client();
    let mut routes = backend.route_client();

    let deployment = models
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(local_source()),
            exposure: Some(direct_exposure("public-model")),
            model_deployment_name: None,
        })
        .await
        .expect("Direct load")
        .into_inner()
        .deployment
        .expect("Direct deployment");
    let listed_after_load = routes
        .list_routes(dotcore_proto::dotcore::inference::v1::ListRoutesRequest {})
        .await
        .expect("routes immediately after Direct load")
        .into_inner()
        .routes;
    assert_eq!(
        listed_after_load
            .iter()
            .map(|view| view
                .route
                .as_ref()
                .expect("route declaration")
                .name
                .as_str())
            .collect::<Vec<_>>(),
        ["public-model"]
    );

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id,
        })
        .await
        .expect("Direct unload");
    let listed_after_unload = routes
        .list_routes(dotcore_proto::dotcore::inference::v1::ListRoutesRequest {})
        .await
        .expect("routes immediately after Direct unload")
        .into_inner()
        .routes;
    assert_eq!(listed_after_unload, []);

    backend.stop().await;
}

#[tokio::test]
async fn hidden_load_persists_one_deployment_without_a_route() {
    let root = support::temporary_fast_fixture();
    let service = support::model_service_for(&support::configuration(&root))
        .await
        .expect("Hidden model service");
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(local_source()),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("Hidden load")
    .into_inner()
    .deployment
    .expect("Hidden deployment");
    let site = read_site(&root);

    assert_eq!(site.model_deployments.len(), 1);
    assert_eq!(
        site.model_deployments[0].model_deployment_id,
        deployment.model_deployment_id
    );
    assert_eq!(site.inference_routes, []);
}

#[tokio::test]
async fn local_deployments_are_independent_and_a_conflicting_route_is_atomic() {
    let root = support::temporary_fast_fixture();
    let service = support::model_service_for(&support::configuration(&root))
        .await
        .expect("Model service");
    let first = create_local_deployment(&service, "public-model")
        .await
        .expect("first Direct deployment");
    let second = create_local_deployment(&service, "other-public-model")
        .await
        .expect("second Direct deployment");

    assert_ne!(first.model_deployment_id, second.model_deployment_id);
    assert_eq!(
        first.intent.expect("first intent").placement,
        Some(support::exact_placement("fixture-node", "fixture-target"))
    );
    assert_eq!(
        second.intent.expect("second intent").placement,
        Some(support::exact_placement("fixture-node", "fixture-target"))
    );
    assert_eq!(
        match first
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                local,
            )) => &local.profile_id,
            _ => panic!("first local snapshot"),
        },
        "fixture-profile"
    );
    assert_eq!(
        match second
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                local,
            )) => &local.profile_id,
            _ => panic!("second local snapshot"),
        },
        "fixture-profile"
    );

    let accepted_site = read_site(&root);
    let error = create_local_deployment(&service, "public-model")
        .await
        .expect_err("duplicate exposure route is rejected");
    assert_eq!(error.code(), Code::AlreadyExists);
    assert_eq!(
        error.message(),
        "Inference route name already exists: public-model"
    );
    let mut expected = accepted_site;
    expected
        .model_deployment_identity_allocator
        .as_mut()
        .expect("deployment identity allocator")
        .next_sequence += 1;
    assert_eq!(read_site(&root), expected);
}

#[tokio::test]
async fn unload_deletes_sole_member_routes_and_keeps_unrelated_routes() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let deployment_id = load_hidden_then_drop(&root).await;
    let other_id = load_hidden_then_drop(&root).await;
    let mut site = read_site(&root);
    let unrelated = sole_member_route("route-2", "other-model", &other_id);
    site.inference_routes = vec![
        sole_member_route("route-1", "public-model", &deployment_id),
        unrelated.clone(),
    ];
    write_site(&root, &site);
    let service = support::model_service_for(&configuration)
        .await
        .expect("Model service");

    let unloading = ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        }),
    )
    .await
    .expect("unload with sole-member route")
    .into_inner()
    .deployment
    .expect("unloading deployment");
    assert_eq!(
        unloading.desired_state,
        ModelDeploymentDesiredState::Teardown as i32
    );
    let accepted = read_site(&root);
    assert_eq!(accepted.inference_routes, [unrelated]);
    assert_eq!(accepted.model_deployments.len(), 2);
    assert_eq!(
        accepted
            .model_deployments
            .iter()
            .map(|deployment| (
                deployment.model_deployment_id.as_str(),
                deployment.desired_state
            ))
            .collect::<Vec<_>>(),
        [
            (
                deployment_id.as_str(),
                ModelDeploymentDesiredState::Teardown as i32
            ),
            (
                other_id.as_str(),
                ModelDeploymentDesiredState::Present as i32
            ),
        ]
    );
}

#[tokio::test]
async fn shared_route_blocks_unload_by_name_without_mutation() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let deployment_id = load_hidden_then_drop(&root).await;
    let other_id = load_hidden_then_drop(&root).await;
    let mut site = read_site(&root);
    site.inference_routes = vec![
        sole_member_route("route-1", "public-model", &deployment_id),
        InferenceRoute {
            inference_route_id: "route-2".to_owned(),
            name: "shared-public".to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::RoundRobin as i32,
                model_deployment_ids: vec![deployment_id.clone(), other_id],
            }],
            generation: 1,
        },
    ];
    write_site(&root, &site);
    let service = support::model_service_for(&configuration)
        .await
        .expect("Model service");

    let error = ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id,
        }),
    )
    .await
    .expect_err("shared route blocks unload");

    assert_teardown_blocked_by(&error, "shared-public");
    assert_eq!(read_site(&root), site);
}

#[tokio::test]
async fn route_edit_accepted_before_unload_blocks_the_unload_by_name() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = create_deployment(&backend, direct_exposure("public-model")).await;
    let other_id = create_deployment(&backend, support::hidden_model_exposure()).await;
    let mut routes = backend.route_client();
    let auto_created = only_route(&mut routes).await;

    routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: auto_created.inference_route_id,
            expected_generation: auto_created.generation,
            name: auto_created.name,
            enabled: true,
            fallback_groups: vec![single_group(&deployment_id), single_group(&other_id)],
        })
        .await
        .expect("route edit adds a fallback deployment");
    let before = read_site(&root);
    let error = backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id,
        })
        .await
        .expect_err("edited route blocks its model teardown");

    assert_teardown_blocked_by(&error, "public-model");
    assert_eq!(read_site(&root), before);
    backend.stop().await;
}

#[tokio::test]
async fn unload_accepted_before_route_edit_rejects_the_edit() {
    let root = support::temporary_fast_fixture();
    let backend = start_with_pending_teardowns(&root).await;
    let deployment_id = create_deployment(&backend, direct_exposure("public-model")).await;
    support::wait_for_ready_deployment(&backend, &deployment_id).await;
    let other_id = create_deployment(&backend, support::hidden_model_exposure()).await;
    let mut routes = backend.route_client();
    let auto_created = only_route(&mut routes).await;

    backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("teardown removes its sole-member route");
    let before = read_site(&root);
    let error = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: auto_created.inference_route_id,
            expected_generation: auto_created.generation,
            name: auto_created.name,
            enabled: true,
            fallback_groups: vec![single_group(&deployment_id), single_group(&other_id)],
        })
        .await
        .expect_err("edit of a removed route is rejected");

    assert_eq!(error.code(), Code::NotFound);
    assert_eq!(error.message(), "Inference route was not found");
    assert_eq!(before.inference_routes, []);
    assert_eq!(read_site(&root), before);
    backend.stop().await;
}

#[tokio::test]
async fn sole_member_route_created_before_unload_is_deleted_with_it() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = create_deployment(&backend, support::hidden_model_exposure()).await;
    let mut routes = backend.route_client();

    routes
        .create_route(CreateRouteRequest {
            name: "late-route".to_owned(),
            enabled: true,
            fallback_groups: vec![single_group(&deployment_id)],
        })
        .await
        .expect("sole-member route creation");
    backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id,
        })
        .await
        .expect("teardown removes the late sole-member route");

    assert_eq!(read_site(&root).inference_routes, []);
    assert_eq!(
        routes
            .list_routes(ListRoutesRequest {})
            .await
            .expect("routes after teardown")
            .into_inner()
            .routes,
        []
    );
    backend.stop().await;
}

#[tokio::test]
async fn unload_accepted_before_route_creation_rejects_the_route() {
    let root = support::temporary_fast_fixture();
    let backend = start_with_pending_teardowns(&root).await;
    let deployment_id = create_deployment(&backend, support::hidden_model_exposure()).await;
    support::wait_for_ready_deployment(&backend, &deployment_id).await;
    let mut routes = backend.route_client();

    backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("teardown without routes");
    let before = read_site(&root);
    let error = routes
        .create_route(CreateRouteRequest {
            name: "late-route".to_owned(),
            enabled: true,
            fallback_groups: vec![single_group(&deployment_id)],
        })
        .await
        .expect_err("route to a tearing-down deployment is rejected");

    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(error.message(), "Inference route declaration is invalid");
    assert_eq!(read_site(&root), before);
    backend.stop().await;
}

#[tokio::test]
async fn initial_site_configuration_cannot_bypass_deployment_exposure_commands() {
    let root = support::temporary_fast_fixture();
    let deployment_id = load_hidden_then_drop(&root).await;
    let mut site = read_site(&root);
    site.inference_routes = vec![sole_member_route("route-1", "public-model", &deployment_id)];
    write_site(&root, &site);

    let error = SiteInputBundleValidator::new(
        dotcore::model::engine::declaration_validator(),
        dotcore::model::site_declaration_rules(),
    )
    .validate(&root)
    .expect_err("initial configuration cannot author runtime records");

    assert!(matches!(
        error,
        SiteInputValidationError::RuntimeDeclarations
    ));
    assert_eq!(read_site(&root), site);
}

async fn create_local_deployment(
    service: &SelectedModelService,
    route_name: &str,
) -> Result<ModelDeployment, tonic::Status> {
    ModelServiceContract::create_model_deployment(
        service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(local_source()),
            exposure: Some(direct_exposure(route_name)),
            model_deployment_name: None,
        }),
    )
    .await
    .map(|response| {
        response
            .into_inner()
            .deployment
            .expect("accepted deployment")
    })
}

/// Starts a backend whose runtime fails every stop, so the accepted teardown of a READY
/// deployment stays pending and the desired site keeps its teardown declaration.
async fn start_with_pending_teardowns(root: &std::path::Path) -> support::RunningBackend {
    support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(root),
        SimulatedRuntimeTestSupport::with_stop_failures(usize::MAX),
    )
    .await
}

async fn create_deployment(
    backend: &support::RunningBackend,
    exposure: ModelDeploymentExposure,
) -> String {
    backend
        .model_client()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(local_source()),
            exposure: Some(exposure),
            model_deployment_name: None,
        })
        .await
        .expect("model deployment")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id
}

async fn only_route(
    routes: &mut dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient<
        tonic::transport::Channel,
    >,
) -> InferenceRoute {
    let listed = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list routes")
        .into_inner()
        .routes;
    assert_eq!(listed.len(), 1);
    listed
        .into_iter()
        .next()
        .and_then(|view| view.route)
        .expect("route declaration")
}

async fn load_hidden_then_drop(root: &std::path::Path) -> String {
    let service = support::model_service_for(&support::configuration(root))
        .await
        .expect("Model service");
    let deployment_id = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(local_source()),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("Hidden load")
    .into_inner()
    .deployment
    .expect("Hidden deployment")
    .model_deployment_id;
    drop(service);
    deployment_id
}

fn local_source() -> dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source {
    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
        CreateLocalModelDeploymentRequest {
            model_id: "fixture-model".to_owned(),
            profile_id: "fixture-profile".to_owned(),
            placement: Some(support::exact_placement("fixture-node", "fixture-target")),
            configuration: Some(support::local_model_configuration(
                "fixture-artifact",
                32_768,
            )),
        },
    )
}

fn direct_exposure(route_name: &str) -> ModelDeploymentExposure {
    ModelDeploymentExposure {
        kind: Some(model_deployment_exposure::Kind::Direct(
            DirectRouteExposure {
                route_name: route_name.to_owned(),
            },
        )),
    }
}

fn sole_member_route(route_id: &str, name: &str, deployment_id: &str) -> InferenceRoute {
    InferenceRoute {
        inference_route_id: route_id.to_owned(),
        name: name.to_owned(),
        enabled: true,
        fallback_groups: vec![single_group(deployment_id)],
        generation: 1,
    }
}

fn single_group(deployment_id: &str) -> FallbackGroup {
    FallbackGroup {
        selection: FallbackGroupSelection::Single as i32,
        model_deployment_ids: vec![deployment_id.to_owned()],
    }
}

fn assert_teardown_blocked_by(error: &tonic::Status, route_name: &str) {
    assert_eq!(error.code(), Code::FailedPrecondition);
    assert!(
        error.message().contains(route_name),
        "teardown error must name {route_name:?}, got {:?}",
        error.message()
    );
    assert_ne!(error.message(), "Model lifecycle configuration failed");
}

fn read_site(root: &std::path::Path) -> Site {
    dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("desired site")
}

fn write_site(root: &std::path::Path, site: &Site) {
    fs::write(
        root.join("site.yaml"),
        dotcore_proto::yaml::to_string(site).expect("desired site YAML"),
    )
    .expect("desired site fixture");
}
