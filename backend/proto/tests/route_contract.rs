//! Inference-route protobuf contract tests.
//!
//! These tests pin public inference route declarations, management mutations,
//! and transient availability projections. Route-manager behavior and serving
//! remain outside this contract suite.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        catalog::v1::ModelInputModality,
        inference::v1::{
            CreateRouteRequest, FallbackGroup, FallbackGroupAvailability, FallbackGroupSelection,
            GetRouteRequest, InferenceRoute, InferenceRouteAvailability,
            InferenceRouteMemberAvailability, InferenceRouteMemberExclusionReason,
            InferenceRouteUnavailabilityReason, InferenceRouteView, ListRoutesResponse,
            ReplaceRouteRequest,
        },
    },
};
use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};

#[test]
fn generated_route_bindings_preserve_desired_and_transient_values() {
    let route = InferenceRoute {
        inference_route_id: "route-01".to_owned(),
        name: "my-model".to_owned(),
        enabled: true,
        fallback_groups: vec![FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec!["deployment-01".to_owned()],
        }],
        generation: 1,
    };
    let view = InferenceRouteView {
        route: Some(route),
        availability: Some(InferenceRouteAvailability {
            available: true,
            available_input_modalities: vec![
                ModelInputModality::Text as i32,
                ModelInputModality::Image as i32,
            ],
            exclusion_reasons: vec![],
            fallback_groups: vec![FallbackGroupAvailability {
                fallback_group_index: 0,
                members: vec![InferenceRouteMemberAvailability {
                    model_deployment_id: "deployment-01".to_owned(),
                    eligible: true,
                    available_input_modalities: vec![ModelInputModality::Text as i32],
                    exclusion_reasons: vec![],
                }],
            }],
        }),
        request_kinds: Vec::new(),
    };
    let unavailable_view = InferenceRouteView {
        route: Some(InferenceRoute {
            inference_route_id: "route-02".to_owned(),
            name: "disabled-model".to_owned(),
            enabled: false,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::RoundRobin as i32,
                model_deployment_ids: vec!["deployment-02".to_owned(), "deployment-03".to_owned()],
            }],
            generation: 8,
        }),
        availability: Some(InferenceRouteAvailability {
            available: false,
            available_input_modalities: vec![],
            exclusion_reasons: vec![InferenceRouteUnavailabilityReason::Disabled as i32],
            fallback_groups: vec![FallbackGroupAvailability {
                fallback_group_index: 0,
                members: vec![
                    InferenceRouteMemberAvailability {
                        model_deployment_id: "deployment-02".to_owned(),
                        eligible: false,
                        available_input_modalities: vec![],
                        exclusion_reasons: vec![
                            InferenceRouteMemberExclusionReason::ModelDeploymentNotReady as i32,
                        ],
                    },
                    InferenceRouteMemberAvailability {
                        model_deployment_id: "deployment-03".to_owned(),
                        eligible: true,
                        available_input_modalities: vec![ModelInputModality::Image as i32],
                        exclusion_reasons: vec![],
                    },
                ],
            }],
        }),
        request_kinds: Vec::new(),
    };
    let create = CreateRouteRequest {
        name: "my-model".to_owned(),
        enabled: true,
        fallback_groups: vec![FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec!["deployment-01".to_owned()],
        }],
    };
    let replace = ReplaceRouteRequest {
        inference_route_id: "route-01".to_owned(),
        expected_generation: 1,
        name: "renamed-model".to_owned(),
        enabled: false,
        fallback_groups: vec![FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec!["deployment-01".to_owned()],
        }],
    };
    let get = GetRouteRequest {
        inference_route_id: "route-01".to_owned(),
    };
    let list = ListRoutesResponse {
        routes: vec![view.clone(), unavailable_view.clone()],
    };

    assert_eq!(
        InferenceRouteView::decode(view.encode_to_vec().as_slice())
            .expect("the available route view fixture must decode"),
        view,
    );
    assert_eq!(
        InferenceRouteView::decode(unavailable_view.encode_to_vec().as_slice())
            .expect("the unavailable route view fixture must decode"),
        unavailable_view,
    );
    assert_eq!(
        CreateRouteRequest::decode(create.encode_to_vec().as_slice())
            .expect("the create route fixture must decode"),
        create,
    );
    assert_eq!(
        ReplaceRouteRequest::decode(replace.encode_to_vec().as_slice())
            .expect("the replace route fixture must decode"),
        replace,
    );
    assert_eq!(
        GetRouteRequest::decode(get.encode_to_vec().as_slice())
            .expect("the get route fixture must decode"),
        get,
    );
    assert_eq!(
        ListRoutesResponse::decode(list.encode_to_vec().as_slice())
            .expect("the list route fixture must decode"),
        list,
    );
}

#[test]
fn route_descriptors_define_management_and_availability_without_private_details() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let routes = descriptor_file(&descriptor, "dotcore/inference/v1/routes.proto");
    let site = descriptor_file(&descriptor, "dotcore/site/v1/site.proto");
    let current = descriptor_file(&descriptor, "dotcore/site/store/v1/snapshot.proto");

    assert_eq!(
        service_methods(routes, "RouteService"),
        [
            (
                "ListRoutes",
                ".dotcore.inference.v1.ListRoutesRequest",
                ".dotcore.inference.v1.ListRoutesResponse",
            ),
            (
                "GetRoute",
                ".dotcore.inference.v1.GetRouteRequest",
                ".dotcore.inference.v1.GetRouteResponse",
            ),
            (
                "CreateRoute",
                ".dotcore.inference.v1.CreateRouteRequest",
                ".dotcore.inference.v1.CreateRouteResponse",
            ),
            (
                "ReplaceRoute",
                ".dotcore.inference.v1.ReplaceRouteRequest",
                ".dotcore.inference.v1.ReplaceRouteResponse",
            ),
            (
                "DeleteRoute",
                ".dotcore.inference.v1.DeleteRouteRequest",
                ".dotcore.inference.v1.DeleteRouteResponse",
            ),
        ]
    );
    assert_eq!(
        field_numbers(routes, "InferenceRoute"),
        [
            ("inference_route_id", 1),
            ("name", 2),
            ("enabled", 3),
            ("fallback_groups", 5),
            ("generation", 6),
        ]
    );
    assert_eq!(field_numbers(routes, "ListRoutesResponse"), [("routes", 1)]);
    assert_eq!(
        field_numbers(routes, "GetRouteRequest"),
        [("inference_route_id", 1)]
    );
    assert_eq!(field_numbers(routes, "GetRouteResponse"), [("route", 1)]);
    assert_eq!(
        field_numbers(routes, "CreateRouteRequest"),
        [("name", 1), ("enabled", 2), ("fallback_groups", 4),]
    );
    assert_eq!(field_numbers(routes, "CreateRouteResponse"), [("route", 1)]);
    assert_eq!(
        field_numbers(routes, "ReplaceRouteRequest"),
        [
            ("inference_route_id", 1),
            ("expected_generation", 2),
            ("name", 3),
            ("enabled", 4),
            ("fallback_groups", 6),
        ]
    );
    assert_eq!(
        field_numbers(routes, "ReplaceRouteResponse"),
        [("route", 1)]
    );
    assert_eq!(
        field_numbers(routes, "DeleteRouteRequest"),
        [("inference_route_id", 1), ("expected_generation", 2)]
    );
    assert_eq!(field_numbers(routes, "DeleteRouteResponse"), []);
    assert_eq!(
        field_numbers(routes, "InferenceRouteView"),
        [("route", 1), ("availability", 2), ("request_kinds", 3)]
    );
    assert_eq!(
        field_numbers(routes, "InferenceRouteAvailability"),
        [
            ("available", 1),
            ("available_input_modalities", 2),
            ("exclusion_reasons", 3),
            ("fallback_groups", 4),
        ]
    );
    assert_eq!(
        field_numbers(routes, "FallbackGroupAvailability"),
        [("fallback_group_index", 1), ("members", 2)]
    );
    assert_eq!(
        field_numbers(routes, "InferenceRouteMemberAvailability"),
        [
            ("model_deployment_id", 1),
            ("eligible", 2),
            ("available_input_modalities", 3),
            ("exclusion_reasons", 4),
        ]
    );
    assert_eq!(
        enum_values(routes, "InferenceRouteUnavailabilityReason"),
        [
            ("INFERENCE_ROUTE_UNAVAILABILITY_REASON_UNSPECIFIED", 0),
            ("INFERENCE_ROUTE_UNAVAILABILITY_REASON_DISABLED", 1),
            (
                "INFERENCE_ROUTE_UNAVAILABILITY_REASON_NO_ELIGIBLE_TEXT_MEMBER",
                2
            ),
        ]
    );
    assert_eq!(
        enum_values(routes, "InferenceRouteMemberExclusionReason"),
        [
            ("INFERENCE_ROUTE_MEMBER_EXCLUSION_REASON_UNSPECIFIED", 0),
            (
                "INFERENCE_ROUTE_MEMBER_EXCLUSION_REASON_MODEL_DEPLOYMENT_NOT_READY",
                1
            ),
        ]
    );
    assert!(
        !routes
            .enum_type
            .iter()
            .any(|enum_type| enum_type.name.as_deref() == Some("InferenceRouteKind")),
        "routes have no declaration kind"
    );
    assert_eq!(field_number(site, "Site", "inference_routes"), 6);
    assert_eq!(
        field_type_name(site, "Site", "inference_routes"),
        ".dotcore.inference.v1.InferenceRoute"
    );
    assert!(
        !field_numbers(current, "SiteCurrentState")
            .iter()
            .any(|(name, _)| name.contains("route") || name.contains("availability")),
        "transient route availability must not enter current state"
    );

    for private_name in [
        "target_generation",
        "endpoint",
        "credential",
        "provider",
        "runtime",
        "container",
        "scheduler",
        "counter",
        "history",
        "request_history",
    ] {
        assert!(
            !include_str!("../dotcore/inference/v1/routes.proto")
                .to_ascii_lowercase()
                .contains(private_name),
            "route contract must exclude {private_name}"
        );
    }
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor must contain {name}"))
}

fn service_methods<'a>(
    file: &'a FileDescriptorProto,
    service_name: &str,
) -> Vec<(&'a str, &'a str, &'a str)> {
    file.service
        .iter()
        .find(|service| service.name.as_deref() == Some(service_name))
        .unwrap_or_else(|| panic!("{service_name} service must exist"))
        .method
        .iter()
        .map(|method| {
            (
                method.name.as_deref().expect("method name"),
                method.input_type.as_deref().expect("input type"),
                method.output_type.as_deref().expect("output type"),
            )
        })
        .collect()
}

fn field_number(file: &FileDescriptorProto, message_name: &str, field_name: &str) -> i32 {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some(field_name))
        .unwrap_or_else(|| panic!("{message_name}.{field_name} field must exist"))
        .number
        .expect("field number")
}

fn field_type_name<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> &'a str {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some(field_name))
        .unwrap_or_else(|| panic!("{message_name}.{field_name} field must exist"))
        .type_name
        .as_deref()
        .expect("field type name")
}

fn field_numbers<'a>(file: &'a FileDescriptorProto, message_name: &str) -> Vec<(&'a str, i32)> {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .map(|field| {
            (
                field.name.as_deref().expect("field name"),
                field.number.expect("field number"),
            )
        })
        .collect()
}

fn enum_values<'a>(file: &'a FileDescriptorProto, enum_name: &str) -> Vec<(&'a str, i32)> {
    file.enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some(enum_name))
        .unwrap_or_else(|| panic!("{enum_name} enum must exist"))
        .value
        .iter()
        .map(|value| {
            (
                value.name.as_deref().expect("enum value name"),
                value.number.expect("enum value number"),
            )
        })
        .collect()
}
