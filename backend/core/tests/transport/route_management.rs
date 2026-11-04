//! Management API contracts for route-manager-owned desired routes and transient availability.

use std::fs;

use dotcore_proto::dotcore::{
    catalog::v1::ModelInputModality,
    inference::v1::{
        CreateRouteRequest, DeleteRouteRequest, FallbackGroup, FallbackGroupSelection,
        GetRouteRequest, InferenceRouteMemberExclusionReason, InferenceRouteUnavailabilityReason,
        ListRoutesRequest, ReplaceRouteRequest,
    },
    model::v1::{
        CreateModelDeploymentRequest, DirectRouteExposure, ModelDeploymentExposure,
        RemoveModelDeploymentRequest, model_deployment_exposure,
    },
    site::store::v1::SiteCurrentState,
    site::v1::Site,
};
use prost::Message;

use crate::support;

#[tokio::test]
async fn route_crud_sorts_by_name_replaces_generations_and_publishes_immutable_snapshots() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = ready_deployment(&backend).await;
    let mut routes = backend.route_client();

    let bravo = create_route(&mut routes, "bravo", true, single_groups(&deployment_id))
        .await
        .expect("create bravo route")
        .into_inner()
        .route
        .expect("bravo route");
    assert_eq!(
        bravo.route.as_ref().expect("bravo declaration").generation,
        1
    );

    let alpha = create_route(&mut routes, "alpha", true, single_groups(&deployment_id))
        .await
        .expect("create alpha route")
        .into_inner()
        .route
        .expect("alpha route");
    let alpha_route = alpha.route.expect("alpha declaration");
    assert!(!alpha_route.inference_route_id.is_empty());
    assert_eq!(alpha_route.name, "alpha");
    assert_eq!(alpha_route.generation, 1);

    let listed = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list routes")
        .into_inner();
    assert_eq!(route_names(&listed.routes), ["alpha", "bravo"]);

    let fetched = routes
        .get_route(GetRouteRequest {
            inference_route_id: alpha_route.inference_route_id.clone(),
        })
        .await
        .expect("get alpha route")
        .into_inner()
        .route
        .expect("fetched alpha route");
    assert_eq!(fetched.route.expect("fetched declaration"), alpha_route);

    let snapshot_before_replace = route_snapshot(&routes).await;
    let replaced = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: alpha_route.inference_route_id.clone(),
            expected_generation: 1,
            name: "charlie".to_owned(),
            enabled: false,
            fallback_groups: single_groups(&deployment_id),
        })
        .await
        .expect("replace alpha route")
        .into_inner()
        .route
        .expect("replaced route");
    let replaced_route = replaced.route.expect("replaced declaration");
    assert_eq!(
        replaced_route.inference_route_id,
        alpha_route.inference_route_id
    );
    assert_eq!(replaced_route.name, "charlie");
    assert!(!replaced_route.enabled);
    assert_eq!(replaced_route.generation, 2);
    assert_eq!(
        replaced
            .availability
            .expect("disabled route availability")
            .exclusion_reasons,
        [InferenceRouteUnavailabilityReason::Disabled as i32]
    );
    assert_eq!(
        route_names(&snapshot_before_replace),
        ["alpha", "bravo"],
        "a published snapshot remains immutable after route replacement"
    );

    let listed = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list replaced routes")
        .into_inner();
    assert_eq!(route_names(&listed.routes), ["bravo", "charlie"]);

    routes
        .delete_route(DeleteRouteRequest {
            inference_route_id: bravo.route.expect("bravo declaration").inference_route_id,
            expected_generation: 1,
        })
        .await
        .expect("delete bravo route");
    let listed = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list deleted routes")
        .into_inner();
    assert_eq!(route_names(&listed.routes), ["charlie"]);

    backend.stop().await;
}

#[tokio::test]
async fn route_availability_is_derived_in_member_order_without_changing_generation() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = ready_deployment(&backend).await;
    let mut routes = backend.route_client();

    let created = create_route(
        &mut routes,
        "available",
        true,
        single_groups(&deployment_id),
    )
    .await
    .expect("create available route")
    .into_inner()
    .route
    .expect("created route");
    let created_route = created.route.expect("created declaration");
    let availability = created.availability.expect("created availability");
    assert!(availability.available);
    assert_eq!(
        availability.available_input_modalities,
        [ModelInputModality::Text as i32]
    );
    assert!(availability.exclusion_reasons.is_empty());
    assert_eq!(availability.fallback_groups.len(), 1);
    assert_eq!(availability.fallback_groups[0].fallback_group_index, 0);
    assert_eq!(
        availability.fallback_groups[0].members[0].model_deployment_id,
        deployment_id
    );
    assert!(availability.fallback_groups[0].members[0].eligible);
    assert_eq!(
        availability.fallback_groups[0].members[0].available_input_modalities,
        [ModelInputModality::Text as i32]
    );
    assert!(
        availability.fallback_groups[0].members[0]
            .exclusion_reasons
            .is_empty()
    );

    let disabled = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: created_route.inference_route_id,
            expected_generation: created_route.generation,
            name: created_route.name,
            enabled: false,
            fallback_groups: created_route.fallback_groups,
        })
        .await
        .expect("disable route")
        .into_inner()
        .route
        .expect("disabled route");
    let disabled_route = disabled.route.expect("disabled declaration");
    assert_eq!(disabled_route.generation, 2);
    let availability = disabled.availability.expect("disabled availability");
    assert!(!availability.available);
    assert_eq!(
        availability.exclusion_reasons,
        [InferenceRouteUnavailabilityReason::Disabled as i32]
    );
    assert!(availability.fallback_groups[0].members[0].eligible);

    backend.stop().await;
}

#[tokio::test]
async fn route_management_maps_conflicts_validation_and_preconditions_to_exact_statuses() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = ready_deployment(&backend).await;
    let mut routes = backend.route_client();

    let single = create_route(&mut routes, "single", true, single_groups(&deployment_id))
        .await
        .expect("create single route")
        .into_inner()
        .route
        .expect("single route")
        .route
        .expect("single declaration");

    assert_status(
        create_route(&mut routes, "single", true, single_groups(&deployment_id)).await,
        tonic::Code::AlreadyExists,
    );
    assert_status(
        routes
            .get_route(GetRouteRequest {
                inference_route_id: "missing-route".to_owned(),
            })
            .await,
        tonic::Code::NotFound,
    );
    assert_status(
        create_route(&mut routes, "invalid", true, Vec::new()).await,
        tonic::Code::InvalidArgument,
    );
    assert_status(
        routes
            .replace_route(ReplaceRouteRequest {
                inference_route_id: single.inference_route_id.clone(),
                expected_generation: 0,
                name: single.name.clone(),
                enabled: single.enabled,
                fallback_groups: single.fallback_groups.clone(),
            })
            .await,
        tonic::Code::InvalidArgument,
    );

    let replaced = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: single.inference_route_id.clone(),
            expected_generation: single.generation,
            name: "single-renamed".to_owned(),
            enabled: single.enabled,
            fallback_groups: single.fallback_groups.clone(),
        })
        .await
        .expect("replace single route")
        .into_inner()
        .route
        .expect("replaced single route")
        .route
        .expect("replaced single declaration");
    assert_eq!(replaced.generation, 2);
    assert_status(
        routes
            .delete_route(DeleteRouteRequest {
                inference_route_id: replaced.inference_route_id,
                expected_generation: 1,
            })
            .await,
        tonic::Code::FailedPrecondition,
    );

    backend.stop().await;
}

#[tokio::test]
async fn auto_created_route_accepts_a_replacement_with_a_fallback_group() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let primary_id = load_deployment(&backend, Some(direct_exposure("public-model"))).await;
    let fallback_id = load_hidden_deployment(&backend).await;
    let mut routes = backend.route_client();
    let auto_created = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list auto-created route")
        .into_inner()
        .routes
        .into_iter()
        .next()
        .and_then(|view| view.route)
        .expect("auto-created route declaration");
    assert_eq!(auto_created.fallback_groups, single_groups(&primary_id));
    let fallback_groups = vec![
        FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![primary_id.clone()],
        },
        FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![fallback_id.clone()],
        },
    ];

    let replaced = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: auto_created.inference_route_id.clone(),
            expected_generation: auto_created.generation,
            name: "public-model".to_owned(),
            enabled: true,
            fallback_groups: fallback_groups.clone(),
        })
        .await
        .expect("replace auto-created route with a fallback group")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("replaced declaration");

    assert_eq!(replaced.inference_route_id, auto_created.inference_route_id);
    assert_eq!(replaced.generation, auto_created.generation + 1);
    assert_eq!(replaced.fallback_groups, fallback_groups);
    let site: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("persisted site");
    assert_eq!(site.inference_routes, [replaced]);

    backend.stop().await;
}

#[tokio::test]
async fn route_shape_violations_are_invalid_arguments_without_state_change() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = load_hidden_deployment(&backend).await;
    let mut routes = backend.route_client();
    let site_before = fs::read(root.join("site.yaml")).expect("Site before rejected routes");
    let members = |count: usize| {
        (0..count)
            .map(|index| format!("member-{index}"))
            .collect::<Vec<_>>()
    };
    let group =
        |selection: FallbackGroupSelection, model_deployment_ids: Vec<String>| FallbackGroup {
            selection: selection as i32,
            model_deployment_ids,
        };
    let cases = [
        ("no-groups", Vec::new()),
        (
            "nine-groups",
            members(9)
                .into_iter()
                .map(|member| group(FallbackGroupSelection::Single, vec![member]))
                .collect(),
        ),
        (
            "single-with-two",
            vec![group(FallbackGroupSelection::Single, members(2))],
        ),
        (
            "round-robin-with-one",
            vec![group(FallbackGroupSelection::RoundRobin, members(1))],
        ),
        (
            "round-robin-with-33",
            vec![group(FallbackGroupSelection::RoundRobin, members(33))],
        ),
        (
            "repeated-member",
            vec![
                group(
                    FallbackGroupSelection::RoundRobin,
                    vec![deployment_id.clone(), "member-0".to_owned()],
                ),
                group(FallbackGroupSelection::Single, vec![deployment_id.clone()]),
            ],
        ),
    ];

    for (name, fallback_groups) in cases {
        let error = create_route(&mut routes, name, true, fallback_groups)
            .await
            .expect_err("route shape violation must be rejected");
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{name}");
        assert_eq!(
            error.message(),
            "Inference route declaration is invalid",
            "{name}"
        );
    }
    assert_eq!(
        fs::read(root.join("site.yaml")).expect("Site after rejected routes"),
        site_before
    );

    backend.stop().await;
}

#[tokio::test]
async fn route_management_keeps_source_neutral_member_rejections_exact() {
    let root = support::temporary_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = load_hidden_deployment(&backend).await;
    let mut routes = backend.route_client();

    let created = create_route(
        &mut routes,
        "existing-route",
        true,
        single_groups(&deployment_id),
    )
    .await
    .expect("create existing route")
    .into_inner()
    .route
    .expect("created route")
    .route
    .expect("created declaration");
    assert_status(
        create_route(
            &mut routes,
            "existing-route",
            true,
            single_groups(&deployment_id),
        )
        .await,
        tonic::Code::AlreadyExists,
    );
    assert_status(
        create_route(
            &mut routes,
            "duplicate-member",
            true,
            vec![FallbackGroup {
                selection: FallbackGroupSelection::RoundRobin as i32,
                model_deployment_ids: vec![deployment_id.clone(), deployment_id.clone()],
            }],
        )
        .await,
        tonic::Code::InvalidArgument,
    );
    assert_status(
        create_route(
            &mut routes,
            "missing-member",
            true,
            single_groups("missing-deployment"),
        )
        .await,
        tonic::Code::InvalidArgument,
    );
    assert_status(
        create_route(&mut routes, "invalid-shape", true, Vec::new()).await,
        tonic::Code::InvalidArgument,
    );

    routes
        .delete_route(DeleteRouteRequest {
            inference_route_id: created.inference_route_id,
            expected_generation: created.generation,
        })
        .await
        .expect("remove route before deployment teardown");
    backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("accept deployment teardown");
    assert_status(
        create_route(
            &mut routes,
            "teardown-member",
            true,
            single_groups(&deployment_id),
        )
        .await,
        tonic::Code::InvalidArgument,
    );
    backend.stop().await;
}

#[tokio::test]
async fn unavailable_targets_project_only_lean_not_ready_reasons() {
    let root = support::temporary_fixture();
    let runtime = support::simulated_model_transition_test_support();
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let deployment_id = load_hidden_deployment(&backend).await;

    // Hold the deployment just before Ready so the route is projected while still warming,
    // regardless of scheduler speed.
    tokio::time::timeout(
        support::DIAGNOSTIC_DEADLOCK_WATCHDOG,
        runtime.wait_for_preparation_gate(),
    )
    .await
    .expect("model preparation gate deadlock watchdog");
    runtime.release_preparation_gate();

    tokio::time::timeout(
        support::DIAGNOSTIC_DEADLOCK_WATCHDOG,
        runtime.wait_for_readiness_gate(),
    )
    .await
    .expect("model readiness gate deadlock watchdog");

    let mut routes = backend.route_client();

    let route = create_route(&mut routes, "warming", true, single_groups(&deployment_id))
        .await
        .expect("create route with warming target")
        .into_inner()
        .route
        .expect("warming route");
    let availability = route.availability.expect("warming availability");
    assert!(!availability.available);
    assert!(availability.available_input_modalities.is_empty());
    assert_eq!(
        availability.exclusion_reasons,
        [InferenceRouteUnavailabilityReason::NoEligibleTextMember as i32]
    );
    assert_eq!(availability.fallback_groups.len(), 1);
    assert_eq!(availability.fallback_groups[0].fallback_group_index, 0);
    assert_eq!(
        availability.fallback_groups[0].members[0].model_deployment_id,
        deployment_id
    );
    assert!(!availability.fallback_groups[0].members[0].eligible);
    assert!(
        availability.fallback_groups[0].members[0]
            .available_input_modalities
            .is_empty()
    );
    assert_eq!(
        availability.fallback_groups[0].members[0].exclusion_reasons,
        [InferenceRouteMemberExclusionReason::ModelDeploymentNotReady as i32]
    );

    // Warming assertions are done; let the deployment become Ready.
    runtime.release_readiness_gate();
    support::wait_for_ready_deployment(&backend, &deployment_id).await;
    let ready_route = routes
        .get_route(GetRouteRequest {
            inference_route_id: route
                .route
                .as_ref()
                .expect("warming declaration")
                .inference_route_id
                .clone(),
        })
        .await
        .expect("get ready route")
        .into_inner()
        .route
        .expect("ready route");
    assert_eq!(
        ready_route.route.expect("ready declaration").generation,
        1,
        "availability changes must not change desired route generation"
    );
    assert!(
        ready_route
            .availability
            .expect("ready availability")
            .available
    );

    backend.stop().await;
}

#[tokio::test]
async fn accepted_route_persistence_failures_are_unavailable_and_retain_inspectable_memory() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = ready_deployment(&backend).await;
    let site_path = root.join("site.yaml");
    let interrupted_site_path = root.join("site.yaml.interrupted");
    fs::rename(&site_path, &interrupted_site_path).expect("hide persisted site file");
    fs::create_dir(&site_path).expect("replace persisted site file with a directory");
    let mut routes = backend.route_client();

    let error = create_route(
        &mut routes,
        "persistence-failure",
        true,
        single_groups(&deployment_id),
    )
    .await
    .expect_err("accepted route persistence failure");
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert!(error.message().contains("inspect current state"));

    fs::remove_dir(&site_path).expect("remove injected persistence failure");
    fs::rename(&interrupted_site_path, &site_path).expect("restore persisted site file");
    let routes = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("accepted route remains inspectable")
        .into_inner()
        .routes;
    assert_eq!(route_names(&routes), ["persistence-failure"]);

    backend.stop().await;
}

#[tokio::test]
async fn route_mutations_do_not_write_transient_availability_to_current_state_or_site_records() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let deployment_id = ready_deployment(&backend).await;
    let current_before =
        fs::read(root.join("state.pb")).expect("current state before route mutation");
    let mut routes = backend.route_client();

    create_route(
        &mut routes,
        "durable-route",
        true,
        single_groups(&deployment_id),
    )
    .await
    .expect("create durable route");

    let current_after =
        fs::read(root.join("state.pb")).expect("current state after route mutation");
    assert_eq!(
        SiteCurrentState::decode(current_before.as_slice()).expect("current state before"),
        SiteCurrentState::decode(current_after.as_slice()).expect("current state after"),
        "route availability and serving history must not become current-state records"
    );
    let site: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("persisted site");
    assert_eq!(site.inference_routes.len(), 1);
    let route = &site.inference_routes[0];
    assert_eq!(route.name, "durable-route");
    assert_eq!(route.generation, 1);
    assert_eq!(route.fallback_groups, single_groups(&deployment_id));

    backend.stop().await;
}

async fn ready_deployment(backend: &support::RunningBackend) -> String {
    let deployment_id = load_hidden_deployment(backend).await;
    support::wait_for_ready_deployment(backend, &deployment_id).await;
    deployment_id
}

async fn load_hidden_deployment(backend: &support::RunningBackend) -> String {
    load_deployment(backend, None).await
}

async fn load_deployment(
    backend: &support::RunningBackend,
    exposure: Option<ModelDeploymentExposure>,
) -> String {
    backend
        .model_client()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(exposure.unwrap_or_else(support::hidden_model_exposure)),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded deployment")
        .model_deployment_id
}

async fn create_route(
    routes: &mut dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient<
        tonic::transport::Channel,
    >,
    name: &str,
    enabled: bool,
    fallback_groups: Vec<FallbackGroup>,
) -> Result<
    tonic::Response<dotcore_proto::dotcore::inference::v1::CreateRouteResponse>,
    tonic::Status,
> {
    routes
        .create_route(CreateRouteRequest {
            name: name.to_owned(),
            enabled,
            fallback_groups,
        })
        .await
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

fn single_groups(deployment_id: &str) -> Vec<FallbackGroup> {
    vec![FallbackGroup {
        selection: FallbackGroupSelection::Single as i32,
        model_deployment_ids: vec![deployment_id.to_owned()],
    }]
}

async fn route_snapshot(
    routes: &dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient<
        tonic::transport::Channel,
    >,
) -> Vec<dotcore_proto::dotcore::inference::v1::InferenceRouteView> {
    let mut routes = routes.clone();
    routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("snapshot routes")
        .into_inner()
        .routes
}

fn route_names(routes: &[dotcore_proto::dotcore::inference::v1::InferenceRouteView]) -> Vec<&str> {
    routes
        .iter()
        .map(|view| {
            view.route
                .as_ref()
                .expect("route declaration")
                .name
                .as_str()
        })
        .collect()
}

fn assert_status<T: std::fmt::Debug>(result: Result<T, tonic::Status>, expected: tonic::Code) {
    assert_eq!(
        result.expect_err("route request must fail").code(),
        expected
    );
}
